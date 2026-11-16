//! Synthetic PTY host over the private wire; no daemon, no live sessions.
use super::*;
use crate::session_host::entry::Client;
use crate::session_host::human::{Cut, GrantState, Raw, Retire};
use crate::session_host::result::Delivery;
use portable_pty::CommandBuilder;
use remuda_core::Size;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    host: Arc<Owned>,
    server: Arc<Server>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "rm-human-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let endpoint = Endpoint::bind(&root.join("host")).unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            "stty -echo; while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$EFFECTS\"; done",
        ]);
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("EFFECTS", root.join("effects"));
        command.cwd(&root);
        let host = Arc::new(
            Owned::spawn([4; 16], "synthetic".into(), command, Size::new(80, 24)).unwrap(),
        );
        let server = Arc::new(Server::new(endpoint, host.clone()));
        let running = server.clone();
        let thread = Some(std::thread::spawn(move || running.run().unwrap()));
        Self {
            root,
            host,
            server,
            thread,
        }
    }
    fn path(&self) -> PathBuf {
        self.root.join("host")
    }
    fn human(&self) -> Client {
        Client::open_human(&self.path(), self.host.identity()).unwrap()
    }
    fn control(&self) -> Client {
        let mut client =
            Client::open_expected(&self.path(), Role::Control, self.host.identity()).unwrap();
        assert!(matches!(
            client.request(&Request::Acquire).unwrap(),
            Response::Granted { .. }
        ));
        client
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let until = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < until {
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
fn grant(client: &mut Client, id: u8, expected: u64, secret: u8) -> Response {
    client
        .request(&Request::HumanGrant {
            request: [id; 16],
            expected,
            recovery_secret: [secret; 32],
        })
        .unwrap()
}
fn code(response: Response) -> String {
    match response {
        Response::Error { code } => code,
        other => panic!("expected refusal, got {other:?}"),
    }
}

fn idle(client: &mut Client, id: u8, expected: u64) -> Response {
    client
        .request(&Request::HumanIdleGrant {
            request: [id; 16],
            expected,
            recovery_secret: [id; 32],
        })
        .unwrap()
}

/// Exercise the host over its private wire, including the ownership attached
/// to a connection rather than merely a successful RPC response.
fn exercise_idle_contract(path: &std::path::Path, identity: super::super::api::Identity) -> Client {
    let mut first = Client::open_idle_human(path, identity.clone()).unwrap();
    let mut other = Client::open_idle_human(path, identity.clone()).unwrap();
    assert!(matches!(
        idle(&mut first, 1, 0),
        Response::HumanIdleGranted { grant: 1 }
    ));
    assert_eq!(code(idle(&mut other, 2, 1)), "human_grant_occupied");
    assert_eq!(code(idle(&mut other, 1, 0)), "human_grant_not_owner");
    assert!(matches!(
        idle(&mut first, 1, 0),
        Response::HumanIdleGranted { grant: 1 }
    ));
    // A request ID from another grant mode cannot turn a takeover receipt
    // into evidence that an idle-only claim succeeded.
    assert_eq!(code(grant(&mut first, 1, 0, 1)), "human_grant_conflict");
    assert!(matches!(
        grant(&mut other, 3, 1, 3),
        Response::HumanGranted {
            grant: 2,
            state: GrantState::Current
        }
    ));
    assert_eq!(code(idle(&mut first, 1, 0)), "human_grant_stale");
    drop(first); // stale cleanup must not release the successor
    let mut contender = Client::open_idle_human(path, identity.clone()).unwrap();
    assert_eq!(code(idle(&mut contender, 4, 2)), "human_grant_occupied");
    drop(other);
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        match idle(&mut contender, 5, 2) {
            Response::HumanIdleGranted { grant: 3 } => break,
            Response::Error { code } if code == "human_grant_occupied" => {}
            other => panic!("unexpected release outcome: {other:?}"),
        }
        assert!(std::time::Instant::now() < deadline, "holder not released");
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(contender);
    let mut observer = Client::open_observer(path, identity.clone()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        if matches!(observer.request(&Request::Inspect).unwrap(), Response::Info { info } if info.activity.as_ref().is_some_and(|activity| !activity.attached))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "idle grant not released"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut replay = Client::open_idle_human(path, identity.clone()).unwrap();
    assert_eq!(code(idle(&mut replay, 5, 2)), "human_grant_not_owner");
    drop(replay);
    // Two concurrent requests for the same observed generation: one wins.
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (6..8)
        .map(|id| {
            let mut client = Client::open_idle_human(path, identity.clone()).unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let response = idle(&mut client, id, 3);
                (client, response)
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|(_, r)| matches!(r, Response::HumanIdleGranted { grant: 4 }))
            .count(),
        1
    );
    assert_eq!(results.iter().filter(|(_, r)| matches!(r, Response::Error { code } if code == "human_grant_stale" || code == "human_grant_occupied")).count(), 1);
    drop(results);
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        if matches!(observer.request(&Request::Inspect).unwrap(), Response::Info { info } if info.activity.as_ref().is_some_and(|activity| !activity.attached))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "race winner not released"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // The host granted, but this connection never reads its reply. Closing it
    // must release that grant before a later client can claim the slot.
    let mut lost = Client::open_idle_human(path, identity.clone()).unwrap();
    lost.send_fixture(&Request::HumanIdleGrant {
        request: [8; 16],
        expected: 4,
        recovery_secret: [8; 32],
    })
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        if matches!(observer.request(&Request::Inspect).unwrap(), Response::Info { info } if info.human.as_ref().is_some_and(|human| human.high_water == 5))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "unread grant was not processed"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(lost);
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        if matches!(observer.request(&Request::Inspect).unwrap(), Response::Info { info } if info.activity.as_ref().is_some_and(|activity| !activity.attached))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "unread grant not released"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let (holder, grant) = Client::open_idle_human(path, identity)
        .unwrap()
        .claim_idle(5)
        .unwrap();
    assert_eq!(grant, 6);
    holder
}

#[test]
fn idle_grant_wire_preserves_owner_and_bounds_replay() {
    let fixture = Fixture::new();
    drop(exercise_idle_contract(
        &fixture.path(),
        fixture.host.identity(),
    ));
}

#[test]
fn idle_client_fails_closed_on_old_host_and_uncertain_reply() {
    use super::super::api::{Identity, Open};
    let root = std::env::temp_dir().join(format!(
        "rm-idle-mock-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    let identity = Identity {
        host: [9; 16],
        session: "test".into(),
        instance: "one".into(),
    };
    for scenario in 0..3 {
        let path = root.join(format!("host-{scenario}"));
        let endpoint = Endpoint::bind(&path).unwrap();
        let expected_identity = identity.clone();
        let server = std::thread::spawn(move || {
            let mut channel = endpoint.accept().unwrap();
            let open: Open = serde_json::from_slice(&channel.receive().unwrap()).unwrap();
            assert!(open
                .protocol
                .required
                .iter()
                .any(|cap| cap == super::super::api::HUMAN_IDLE_GRANT));
            if scenario == 0 {
                // An older host refuses the required unknown capability.
                channel
                    .send(
                        &serde_json::to_vec(&Response::Error {
                            code: "unsupported_protocol_or_capability".into(),
                        })
                        .unwrap(),
                    )
                    .unwrap();
                assert!(
                    channel.receive().is_err(),
                    "client sent a grant to the old host"
                );
                return;
            }
            channel
                .send(
                    &serde_json::to_vec(&Response::Opened {
                        identity: expected_identity,
                        minor: 0,
                        capabilities: vec![
                            "inspect".into(),
                            super::super::api::HUMAN_INPUT.into(),
                            super::super::api::HUMAN_IDLE_GRANT.into(),
                        ],
                        max_payload: 65_519,
                    })
                    .unwrap(),
                )
                .unwrap();
            let request: Request = serde_json::from_slice(&channel.receive().unwrap()).unwrap();
            assert!(matches!(
                request,
                Request::HumanIdleGrant { expected: 0, .. }
            ));
            if scenario == 2 {
                channel
                    .send(&serde_json::to_vec(&Response::Ok).unwrap())
                    .unwrap();
                assert!(
                    channel.receive().is_err(),
                    "client retained an unproven grant"
                );
            }
            // Scenario 1 closes without a reply, making the outcome uncertain.
        });
        let opened = Client::open_idle_human(&path, identity.clone());
        if scenario == 0 {
            assert!(opened.is_err());
        } else {
            assert!(opened.unwrap().claim_idle(0).is_err());
        }
        server.join().unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

/// Run explicitly with REMUDA_TEST_HOST_BIN pointing at the freshly built
/// standalone host. This covers a real host process rather than a server thread.
#[test]
#[ignore = "requires a freshly built standalone host in REMUDA_TEST_HOST_BIN"]
fn idle_grant_against_standalone_host_process() {
    use std::process::{Command, Stdio};
    struct Process(std::process::Child, PathBuf);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    #[cfg(target_os = "macos")]
    let base = "/private/tmp";
    #[cfg(not(target_os = "macos"))]
    let base = "/tmp";
    let root = PathBuf::from(base).join(format!(
        "rm-idle-process-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    let endpoint = root.join("host");
    let child = Command::new(std::env::var_os("REMUDA_TEST_HOST_BIN").expect("host binary"))
        .arg("run")
        .arg(&endpoint)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut process = Process(child, root.clone());
    let launch = serde_json::json!({"name":"idle-test", "argv":["/bin/sh"], "cwd":root, "agent":"synthetic", "rows":24, "cols":80, "env":[["PATH","/usr/bin:/bin"]]});
    super::super::wire::write_bootstrap(
        process.0.stdin.as_mut().unwrap(),
        &serde_json::to_vec(&launch).unwrap(),
    )
    .unwrap();
    let _ = super::super::wire::read_frame(process.0.stdout.as_mut().unwrap(), 8192).unwrap();
    let identity = super::super::entry::manifest(&endpoint).unwrap().identity;
    let mut holder = exercise_idle_contract(&endpoint, identity);
    assert!(
        process.0.try_wait().unwrap().is_none(),
        "host exited during test"
    );
    process.0.kill().unwrap();
    process.0.wait().unwrap();
    assert!(
        holder.request(&Request::Inspect).is_err(),
        "dead host must not look like a live grant"
    );
}

#[test]
fn human_role_needs_negotiated_capability_and_view_cannot_grant() {
    let fixture = Fixture::new();
    assert!(
        Client::open_human_without_capability(&fixture.path(), fixture.host.identity()).is_err()
    );
    let mut view =
        Client::open_expected(&fixture.path(), Role::View, fixture.host.identity()).unwrap();
    assert_eq!(code(grant(&mut view, 1, 0, 1)), "human_role_required");
    assert_eq!(code(idle(&mut view, 1, 0)), "human_role_required");
    assert_eq!(
        code(view.request(&Request::RawCut { last: 1 }).unwrap()),
        "raw_outcome_not_visible"
    );
    let mut control = fixture.control();
    assert_eq!(code(grant(&mut control, 1, 0, 1)), "human_role_required");
    assert_eq!(code(idle(&mut control, 1, 0)), "human_role_required");
}

#[test]
fn checked_grant_uses_core_high_water_and_retry_is_evidence_only() {
    let fixture = Fixture::new();
    let mut first = fixture.human();
    assert_eq!(code(grant(&mut first, 1, 5, 1)), "human_grant_stale");
    assert!(matches!(
        grant(&mut first, 1, 0, 1),
        Response::HumanGranted {
            grant: 1,
            state: GrantState::Current
        }
    ));
    assert!(fixture.host.inspect().is_ok());
    let mut second = fixture.human();
    // Lost-ACK retry from another connection: evidence, never a transfer.
    assert!(matches!(
        grant(&mut second, 1, 0, 1),
        Response::HumanGranted {
            grant: 1,
            state: GrantState::Current
        }
    ));
    assert_eq!(code(grant(&mut second, 1, 0, 2)), "human_grant_conflict");
    assert!(matches!(
        grant(&mut second, 2, 1, 2),
        Response::HumanGranted {
            grant: 2,
            state: GrantState::Current
        }
    ));
    // Only the latest receipt is retained; an older request is just stale.
    assert_eq!(code(grant(&mut first, 1, 0, 1)), "human_grant_stale");
    drop(second);
    eventually_released(&fixture);
    drop(first);
}
fn eventually_released(fixture: &Fixture) {
    let until = std::time::Instant::now() + Duration::from_secs(4);
    while fixture.host.human_attached_fixture() {
        assert!(
            std::time::Instant::now() < until,
            "disconnect kept core guard"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn revoke_and_discard_are_fenced_and_stop_waits_for_them() {
    let fixture = Fixture::new();
    let mut human = fixture.human();
    let mut control = fixture.control();
    assert!(matches!(
        grant(&mut human, 1, 0, 1),
        Response::HumanGranted { grant: 1, .. }
    ));
    fixture
        .host
        .admit_raw_fixture(1, 1, b"draft", Delivery::Written);
    let cut = match human.request(&Request::RawCut { last: 1 }).unwrap() {
        Response::RawCut { cut } => cut,
        other => panic!("{other:?}"),
    };
    let forged = Cut {
        hash: [0; 32],
        ..cut
    };
    assert!(matches!(
        human
            .request(&Request::AcknowledgeRaw { cut: forged })
            .unwrap(),
        Response::Retired {
            state: Retire::Conflict
        }
    ));
    assert!(matches!(
        human
            .request(&Request::RecoverRaw {
                grant: 1,
                sequence: 1,
                bytes: b"draft".to_vec()
            })
            .unwrap(),
        Response::RawOutcome {
            state: Raw::Complete(Delivery::Written)
        }
    ));
    assert!(matches!(
        control.request(&Request::DiscardRaw { cut }).unwrap(),
        Response::Retired {
            state: Retire::Live
        }
    ));
    assert_eq!(
        code(human.request(&Request::RevokeHuman { grant: 1 }).unwrap()),
        "view_connection_cannot_mutate"
    );
    assert!(matches!(
        control.request(&Request::RevokeHuman { grant: 1 }).unwrap(),
        Response::Revoked { revoked: true }
    ));
    assert!(
        !fixture.host.human_attached_fixture(),
        "revoke released core guard"
    );
    assert!(matches!(
        control.request(&Request::DiscardRaw { cut }).unwrap(),
        Response::Retired {
            state: Retire::Retired
        }
    ));
}

fn effects(fixture: &Fixture) -> Vec<String> {
    std::fs::read_to_string(fixture.root.join("effects"))
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}
fn input(client: &mut Client, grant: u64, sequence: u64, bytes: &[u8]) -> Response {
    client
        .request(&Request::HumanInput {
            grant,
            sequence,
            bytes: bytes.to_vec(),
        })
        .unwrap()
}
fn settled(client: &mut Client, grant: u64, sequence: u64, bytes: &[u8]) -> Raw {
    let until = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        let state = match client
            .request(&Request::RecoverRaw {
                grant,
                sequence,
                bytes: bytes.to_vec(),
            })
            .unwrap()
        {
            Response::RawOutcome { state } => state,
            other => panic!("{other:?}"),
        };
        if state != Raw::Pending {
            return state;
        }
        assert!(std::time::Instant::now() < until, "raw write never settled");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn eventually_effects(fixture: &Fixture, expected: &[&str]) {
    let until = std::time::Instant::now() + Duration::from_secs(4);
    while effects(fixture) != expected {
        assert!(
            std::time::Instant::now() < until,
            "effects {:?}",
            effects(fixture)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn human_raw_write_lands_once_and_retry_never_replays() {
    let fixture = Fixture::new();
    let mut human = fixture.human();
    assert!(matches!(
        grant(&mut human, 1, 0, 1),
        Response::HumanGranted { grant: 1, .. }
    ));
    assert!(matches!(
        input(&mut human, 1, 1, b"one\n"),
        Response::RawOutcome {
            state: Raw::New | Raw::Pending
        }
    ));
    assert_eq!(
        settled(&mut human, 1, 1, b"one\n"),
        Raw::Complete(Delivery::Written)
    );
    eventually_effects(&fixture, &["one"]);
    assert!(matches!(
        input(&mut human, 1, 1, b"one\n"),
        Response::RawOutcome {
            state: Raw::Complete(Delivery::Written)
        }
    ));
    assert!(matches!(
        input(&mut human, 1, 1, b"two\n"),
        Response::RawOutcome {
            state: Raw::Conflict
        }
    ));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(effects(&fixture), ["one"], "retry replayed a write");
    assert_eq!(
        code(input(
            &mut human,
            1,
            1,
            &[0; super::super::wire::MAX_INPUT + 1]
        )),
        "invalid_raw_input"
    );
}

#[test]
fn takeover_stales_the_old_grant_and_only_the_new_holder_writes() {
    let fixture = Fixture::new();
    let mut old = fixture.human();
    let mut new = fixture.human();
    assert!(matches!(
        grant(&mut old, 1, 0, 1),
        Response::HumanGranted { grant: 1, .. }
    ));
    input(&mut old, 1, 1, b"old\n");
    assert_eq!(
        settled(&mut old, 1, 1, b"old\n"),
        Raw::Complete(Delivery::Written)
    );
    assert!(matches!(
        grant(&mut new, 2, 1, 2),
        Response::HumanGranted { grant: 2, .. }
    ));
    assert!(matches!(
        input(&mut old, 1, 2, b"late\n"),
        Response::RawOutcome { state: Raw::Stale }
    ));
    assert!(matches!(
        input(&mut new, 2, 2, b"new\n"),
        Response::RawOutcome {
            state: Raw::New | Raw::Pending
        }
    ));
    assert_eq!(
        settled(&mut new, 2, 2, b"new\n"),
        Raw::Complete(Delivery::Written)
    );
    eventually_effects(&fixture, &["old", "new"]);
}
