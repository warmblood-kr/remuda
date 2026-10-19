//! Private synthetic PTY, actual separate controller processes, no daemon.
use super::*;
use crate::session_host::api::Role;
use crate::session_host::{control::ConnectionId, owned::Owned, server::Server, unix::Endpoint};
use portable_pty::CommandBuilder;
use remuda_core::Size;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;
static NEXT: AtomicU64 = AtomicU64::new(0);

fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < until, "private recovery fixture deadline");
        std::thread::sleep(Duration::from_millis(5));
    }
}
struct Fixture {
    root: PathBuf,
    host: Arc<Owned>,
    server: Arc<Server>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "rm-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let endpoint = Endpoint::bind(&root.join("host")).unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "stty -echo; : > \"$READY\"; printf 'ready\\n'; while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$EFFECTS\"; printf 'effect:%s\\n' \"$line\"; done"]);
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("READY", root.join("ready"));
        command.env("EFFECTS", root.join("effects"));
        command.cwd(&root);
        let host = Arc::new(
            Owned::spawn([9; 16], "synthetic".into(), command, Size::new(80, 24)).unwrap(),
        );
        let server = Arc::new(Server::new(endpoint, host.clone()));
        let running = server.clone();
        let thread = Some(std::thread::spawn(move || running.run().unwrap()));
        let fixture = Self {
            root,
            host,
            server,
            thread,
        };
        eventually(|| fixture.root.join("ready").exists().then_some(()));
        fixture.bootstrap();
        fixture
    }
    fn target(&self) -> Target {
        Target {
            name: "synthetic".into(),
            identity: self.host.identity(),
        }
    }
    fn bootstrap(&self) {
        let store = Store::create(&self.root.join("inventory")).unwrap();
        let applied = store
            .commit(
                store.read().unwrap().cursor(),
                None,
                Mutation::Claim { nonce: [1; 16] },
                Duration::ZERO,
            )
            .unwrap();
        let fence = Some(applied.fence);
        let applied = store
            .commit(
                applied.next.cursor(),
                fence,
                Mutation::ReserveLaunch {
                    name: "synthetic".into(),
                    entry: model::Entry::reserved(self.host.identity(), [2; 16], "{}").unwrap(),
                },
                Duration::ZERO,
            )
            .unwrap();
        store
            .commit(
                applied.next.cursor(),
                fence,
                Mutation::Register(self.target()),
                Duration::ZERO,
            )
            .unwrap();
    }
    fn owner(&self) -> Owner {
        Owner::claim(Store::open(&self.root.join("inventory")).unwrap()).unwrap()
    }
    fn key(&self) -> Key {
        let index = Store::open(&self.root.join("inventory"))
            .unwrap()
            .read()
            .unwrap();
        index.entries["synthetic"]
            .pending
            .as_ref()
            .unwrap()
            .key
            .clone()
    }
    fn effects(&self) -> String {
        std::fs::read_to_string(self.root.join("effects")).unwrap_or_default()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            if let Ok(fence) = self.host.acquire(ConnectionId(u128::MAX)) {
                let result = self.host.close(fence);
                let _ = self.host.release(fence);
                if result.is_ok() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.server.stop_fixture();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn controller(root: &Path, stage: u8) -> Process {
    Process(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session_host::recovery::tests::controller_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("REMUDA_TEST_RECOVERY_ROOT", root)
            .env("REMUDA_TEST_RECOVERY_CUT", stage.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}
pub(super) fn cut(stage: u8) {
    if std::env::var("REMUDA_TEST_RECOVERY_CUT").ok().as_deref() != Some(&stage.to_string()) {
        return;
    }
    let root = PathBuf::from(std::env::var_os("REMUDA_TEST_RECOVERY_ROOT").unwrap());
    crate::fs_atomic::write_atomic_lua_private(&root.join("cut"), &[stage]).unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("parent did not kill its synthetic controller");
}
#[test]
#[ignore = "internal synthetic controller helper, invoked by private process fixtures"]
fn controller_fixture() {
    let root =
        PathBuf::from(std::env::var_os("REMUDA_TEST_RECOVERY_ROOT").expect("private fixture root"));
    let store = Store::open(&root.join("inventory")).unwrap();
    let identity = store.read().unwrap().entries["synthetic"].identity.clone();
    let target = || Target {
        name: "synthetic".into(),
        identity: identity.clone(),
    };
    let owner = Owner::claim(store).unwrap();
    let stage = std::env::var("REMUDA_TEST_RECOVERY_CUT")
        .unwrap()
        .parse()
        .unwrap();
    let key = owner
        .reserve(target(), [3; 16], [4; 16], 1, act(stage))
        .unwrap();
    assert!(matches!(
        owner.resume(target(), key, &root.join("host")).unwrap(),
        Progress::Pending
    ));
    panic!("controller did not reach requested deterministic cut");
}
fn exercise_cut(stage: u8) {
    let fixture = Fixture::new();
    let before = fixture.host.inspect().unwrap();
    let mut process = controller(&fixture.root, stage);
    eventually(|| fixture.root.join("cut").exists().then_some(()));
    if stage == 1 {
        assert!(fixture.effects().is_empty());
    }
    if stage == 3 {
        eventually(|| (fixture.effects() == "once\n").then_some(()));
    }
    process.0.kill().unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    while process.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < until, "owned controller did not die");
        std::thread::sleep(Duration::from_millis(5));
    }
    let key = fixture.key();
    assert_eq!(key.payload, act(stage).digest().unwrap());
    let owner = fixture.owner();
    let completion = eventually(|| {
        match owner
            .resume(fixture.target(), key.clone(), &fixture.root.join("host"))
            .unwrap()
        {
            Progress::Complete(completion) => Some(completion),
            Progress::Pending | Progress::Busy | Progress::Unavailable => None,
            _ => panic!("unexpected recovery state"),
        }
    });
    assert_eq!(completion.delivery, super::super::result::Delivery::Written);
    let expected = if stage == 3 {
        "once\nsecond\n"
    } else {
        "once\n"
    };
    eventually(|| (fixture.effects() == expected).then_some(()));
    for _ in 0..100 {
        assert!(matches!(
            fixture
                .owner()
                .resume(fixture.target(), key.clone(), &fixture.root.join("host"))
                .unwrap(),
            Progress::Complete(_)
        ));
    }
    assert_eq!(fixture.effects(), expected);
    let after = fixture.host.inspect().unwrap();
    assert_eq!(before.identity, after.identity);
    assert_eq!(before.pid, after.pid);
    assert!(matches!(
        after.lifecycle,
        super::super::api::Lifecycle::Running
    ));
}
fn act(stage: u8) -> InputAct {
    use super::super::wire::Step;
    if stage == 3 {
        InputAct::Feed(vec![
            Step::Bytes(b"once\n".to_vec()),
            Step::Pause(2000),
            Step::Bytes(b"second\n".to_vec()),
        ])
    } else {
        InputAct::Bytes(b"once\n".to_vec())
    }
}
fn finish(fixture: &Fixture, owner: &Owner, key: &Key) {
    let result = eventually(|| {
        match owner
            .resume(fixture.target(), key.clone(), &fixture.root.join("host"))
            .unwrap()
        {
            Progress::Complete(result) => Some(result),
            Progress::Pending | Progress::Busy | Progress::Unavailable => None,
            _ => panic!("unexpected recovery state"),
        }
    });
    assert_eq!(result.delivery, super::super::result::Delivery::Written);
    eventually(|| (fixture.effects() == "once\n").then_some(()));
}
#[test]
fn stale_writer_changed_identity_and_live_physical_lease_cannot_admit() {
    let fixture = Fixture::new();
    let old = fixture.owner();
    let key = old
        .reserve(fixture.target(), [3; 16], [4; 16], 1, act(1))
        .unwrap();
    let current = fixture.owner();
    assert!(matches!(
        old.resume(fixture.target(), key.clone(), &fixture.root.join("host")),
        Err(Error::Data(model::Error::StaleFence))
    ));
    let mut wrong = fixture.target();
    wrong.identity.instance = "replacement".into();
    assert!(matches!(
        current.resume(wrong, key.clone(), &fixture.root.join("host")),
        Err(Error::Data(model::Error::Conflict))
    ));
    let mut held = Client::open_expected(
        &fixture.root.join("host"),
        Role::Control,
        fixture.host.identity(),
    )
    .unwrap();
    assert!(matches!(
        held.request(&Request::Acquire),
        Ok(Response::Granted { .. })
    ));
    assert!(matches!(
        current
            .resume(fixture.target(), key.clone(), &fixture.root.join("host"))
            .unwrap(),
        Progress::Busy
    ));
    assert!(fixture.effects().is_empty());
    drop(held);
    finish(&fixture, &current, &key);
}
#[test]
fn uncertain_reservation_commit_never_sends_until_explicit_recovery() {
    for stage in [1, 2] {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        super::super::inventory::disk::fail_at(stage);
        assert!(matches!(
            owner.reserve(fixture.target(), [3; 16], [4; 16], 1, act(1)),
            Err(Error::CommitUnknown(_))
        ));
        assert!(fixture.effects().is_empty());
        let key = Key {
            client: [4; 16],
            sequence: 1,
            payload: act(1).digest().unwrap(),
        };
        if stage == 1 {
            assert!(matches!(
                owner
                    .resume(fixture.target(), key, &fixture.root.join("host"))
                    .unwrap(),
                Progress::Indeterminate
            ));
            assert!(fixture.effects().is_empty());
        } else {
            finish(&fixture, &fixture.owner(), &key);
        }
    }
}
#[test]
fn lost_reservation_and_completion_ack_retries_reuse_original_key_without_effect_replay() {
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let key = owner
        .reserve(fixture.target(), [3; 16], [4; 16], 1, act(1))
        .unwrap();
    let retry = fixture.owner();
    assert_eq!(
        retry
            .reserve(fixture.target(), [3; 16], [4; 16], 1, act(1))
            .unwrap(),
        key
    );
    assert!(matches!(
        retry.reserve(fixture.target(), [5; 16], [4; 16], 1, act(1)),
        Err(Error::Data(model::Error::Conflict))
    ));
    assert!(matches!(
        retry.reserve(
            fixture.target(),
            [3; 16],
            [4; 16],
            1,
            InputAct::Bytes(b"changed\n".to_vec())
        ),
        Err(Error::Data(model::Error::Conflict))
    ));
    assert!(fixture.effects().is_empty());
    finish(&fixture, &retry, &key);
    assert_eq!(
        fixture
            .owner()
            .reserve(fixture.target(), [3; 16], [4; 16], 1, act(1))
            .unwrap(),
        key
    );
    assert_eq!(fixture.effects(), "once\n");
}
#[test]
fn same_host_never_admitted_proof_resolves_uncertain_transport_but_not_admitted_contradiction() {
    for phase in [InputPhase::Indeterminate, InputPhase::Admitted] {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        let key = owner
            .reserve(fixture.target(), [3; 16], [4; 16], 1, act(1))
            .unwrap();
        owner
            .commit(
                fixture.target(),
                InputMutation::Observe {
                    key: key.clone(),
                    phase,
                },
            )
            .unwrap();
        if phase == InputPhase::Indeterminate {
            finish(&fixture, &owner, &key);
        } else {
            assert!(matches!(
                owner
                    .resume(fixture.target(), key, &fixture.root.join("host"))
                    .unwrap(),
                Progress::Indeterminate
            ));
            assert!(fixture.effects().is_empty());
        }
    }
}
#[test]
fn controller_death_after_first_effect_drains_original_full_act_and_retains_recovery() {
    exercise_cut(3);
}
#[test]
fn controller_death_after_durable_reservation_before_receipt_recovers_original_without_recreation()
{
    exercise_cut(1);
}
#[test]
fn controller_death_after_host_admission_before_observation_recovers_without_duplicate_effect() {
    exercise_cut(2);
}
