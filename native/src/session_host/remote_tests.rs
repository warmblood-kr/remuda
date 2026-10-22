//! Adapter proofs against a separate host process; controllers die by SIGKILL.
use super::*;
use crate::session_host::birth::{run, Bootstrap};
use crate::session_host::entry::Launch;
use crate::session_host::inventory::disk::Store;
use crate::session_host::inventory::model::{Birth, Entry, Lease, Mutation};
use serde_json::value::RawValue;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
const CLIENT: [u8; 16] = [5; 16];

fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < until, "deadline: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn helper(root: &Path, mode: &str) -> Process {
    Process(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session_host::remote::tests::process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("REMUDA_TEST_REMOTE_ROOT", root)
            .env("REMUDA_TEST_REMOTE_MODE", mode)
            // Only the "reserved" helper parks inside Owner::reserve's cut 1.
            .env("REMUDA_TEST_RECOVERY_ROOT", root)
            .env(
                "REMUDA_TEST_RECOVERY_CUT",
                if mode == "reserved" { "1" } else { "0" },
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}
fn try_adapter(root: &Path) -> Option<HostSession> {
    let store = Store::open(&root.join("inventory")).ok()?;
    let owner = Arc::new(Owner::claim(store).ok()?);
    HostSession::open(owner, "synthetic", CLIENT).ok()
}
fn adapter(root: &Path) -> HostSession {
    try_adapter(root).expect("adapter over the private inventory")
}
#[test]
#[ignore = "internal host/controller helper; invoked only by parent fixtures"]
fn process_fixture() {
    let root = PathBuf::from(std::env::var_os("REMUDA_TEST_REMOTE_ROOT").unwrap());
    match std::env::var("REMUDA_TEST_REMOTE_MODE").unwrap().as_str() {
        "host" => run(&root.join("h")).unwrap(),
        // An old daemon: one long whole act, killed by the parent mid-act.
        // An old daemon parked after the durable reservation, before any RPC.
        "reserved" => {
            let _ = adapter(&root).send_line("alpha");
            panic!("parent did not kill its synthetic controller");
        }
        "controller" => {
            let session = adapter(&root);
            let _ = session.feed(&[
                Step::Burst(b"first\n".to_vec()),
                Step::Pause(1500),
                Step::Burst(b"second\n".to_vec()),
            ]);
            panic!("parent did not kill its synthetic controller");
        }
        mode => panic!("unknown helper mode {mode}"),
    }
}
struct Fixture {
    root: PathBuf,
    host: Option<Process>,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        })
        .join(format!(
            "rha-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let identity = HostIdentity {
            host: [9; 16],
            session: "adapter-session".into(),
            instance: "adapter-instance".into(),
        };
        // Effects are appended once per line the child actually reads.
        let script = "stty -echo; printf 'ready\\n'; while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$EFFECTS\"; printf 'effect:%s\\n' \"$line\"; done";
        let launch = Launch {
            name: "synthetic".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: root.to_string_lossy().into(),
            agent: "synthetic".into(),
            rows: 24,
            cols: 80,
            env: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                (
                    "EFFECTS".into(),
                    root.join("effects").to_string_lossy().into(),
                ),
            ],
        };
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
                identity,
            },
            [4; 16],
            [7; 16],
        );
        let bootstrap = serde_json::to_vec(&Bootstrap {
            store: root.join("inventory"),
            lease,
            launch: RawValue::from_string(raw).unwrap(),
        })
        .unwrap();
        let mut host = helper(&root, "host");
        super::super::wire::write_frame(&mut host.0.stdin.take().unwrap(), &bootstrap).unwrap();
        let fixture = Self {
            root,
            host: Some(host),
        };
        eventually("host ready", || {
            let store = Store::open(&fixture.root.join("inventory")).ok()?;
            let owner = Arc::new(Owner::claim(store).ok()?);
            let session = HostSession::open(owner, "synthetic", CLIENT).ok()?;
            session
                .screen_text()
                .ok()
                .filter(|text| text.contains("ready"))
        });
        fixture
    }
    fn effects(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("effects"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // A test may already have stopped and retired its host.
        if let Some(host) = self.host.as_mut() {
            if matches!(host.0.try_wait(), Ok(Some(_))) {
                let _ = std::fs::remove_dir_all(&self.root);
                return;
            }
        }
        // Exact authenticated cleanup: close the child, then stop the host.
        // The claim may meet store lock contention; retry it briefly.
        let until = Instant::now() + Duration::from_secs(5);
        let session = loop {
            match try_adapter(&self.root) {
                Some(session) => break session,
                None if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
                None => {
                    assert!(std::thread::panicking(), "cleanup adapter unavailable");
                    return;
                }
            }
        };
        let _ = session.terminate();
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            // Stop requires no retained results: receipt any a test left behind.
            if let Ok(index) = session.owner.index() {
                for stream in index
                    .entries
                    .get("synthetic")
                    .iter()
                    .flat_map(|e| &e.streams)
                {
                    for result in stream.results.iter().filter(|r| !r.consumer_ack) {
                        let _ = session.owner.consume(
                            session.target(),
                            result.reservation.key.clone(),
                            result.reservation.intent,
                            &result.completion,
                        );
                    }
                }
            }
            if let Ok(mut client) =
                Client::open_expected(&session.endpoint, Role::Control, session.identity.clone())
            {
                if matches!(
                    client.request(&Request::Acquire),
                    Ok(Response::Granted { .. })
                ) && matches!(client.request(&Request::Stop), Ok(Response::Ok))
                {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Some(mut host) = self.host.take() {
            let exited = (0..200).any(|_| {
                std::thread::sleep(Duration::from_millis(10));
                matches!(host.0.try_wait(), Ok(Some(_)))
            });
            assert!(exited || std::thread::panicking(), "host did not stop");
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn whole_acts_reads_and_resize_go_through_one_separate_host() {
    let fixture = Fixture::new();
    let session = adapter(&fixture.root);
    let before = session.inspect().unwrap();
    assert_eq!(before.lifecycle, Lifecycle::Running);
    assert!(before.pid.is_some());
    assert_eq!(
        before.cut,
        ObservationCut::Host {
            revision: before.output_version.unwrap()
        }
    );
    assert_eq!(session.input_owner(), Some(CLIENT));
    assert_eq!(session.identity().host, Some([9; 16]));

    let version = session.snapshot(0).unwrap().output_version.unwrap();
    session.send_line("one").unwrap();
    assert_eq!(fixture.effects(), ["one"]);
    let after = session.wait_after(version, Duration::from_secs(5)).unwrap();
    assert!(after.output_version.unwrap() > version);
    assert_eq!(after.instance_id.as_deref(), Some("adapter-instance"));
    eventually("rendered effect", || {
        session
            .screen_text()
            .ok()
            .filter(|text| text.contains("effect:one"))
    });
    session.resize(Size::for_pane(100, 30)).unwrap();
    let resized = session.inspect().unwrap().size;
    assert_eq!((resized.cols(), resized.rows()), (100, 30));

    // A consumed result is retired; asking again refuses rather than replaying.
    let index = session.owner.index().unwrap();
    let stream = &index.entries["synthetic"].streams[0];
    assert_eq!(stream.next, Some(2));
    let first = RequestIdentity {
        session: session.identity(),
        client: CLIENT,
        sequence: 1,
        payload: InputAct::SendLine("one".into()).digest().unwrap(),
    };
    assert!(matches!(
        session.recover(&first).unwrap(),
        Recovery::Forgotten
    ));
    assert_eq!(fixture.effects(), ["one"]);
}

#[test]
fn controller_death_mid_act_recovers_the_original_input_without_replay() {
    let fixture = Fixture::new();
    let pid = adapter(&fixture.root).inspect().unwrap().pid;
    let mut old = helper(&fixture.root, "controller");
    // Kill once the act has started; a slow machine may already show second.
    eventually("first effect", || {
        (fixture.effects().first().map(String::as_str) == Some("first")).then_some(())
    });
    old.0.kill().unwrap();
    old.0.wait().unwrap();

    // A new daemon claims the inventory; the child and its terminal survived.
    let session = adapter(&fixture.root);
    let observed = session.inspect().unwrap();
    assert_eq!(observed.pid, pid);
    assert_eq!(observed.lifecycle, Lifecycle::Running);
    let Err(Error::Indeterminate(request)) = session.send_line("third") else {
        panic!("a new act must not start before the old one is recovered");
    };
    assert_eq!(request.client, CLIENT);
    assert_eq!(request.sequence, 1);
    let recovered = eventually("original act completes", || {
        match session.recover(&request) {
            Ok(Recovery::Complete { delivery, reply }) => Some((delivery, reply)),
            Ok(Recovery::Pending) | Err(Error::Unavailable(_)) => None,
            other => panic!("unexpected recovery {other:?}"),
        }
    });
    assert!(matches!(
        recovered,
        (Delivery::Written, RecoveredReply::Unit)
    ));
    assert_eq!(fixture.effects(), ["first", "second"]);

    session.send_line("third").unwrap();
    assert!(matches!(
        session.recover(&request).unwrap(),
        Recovery::Forgotten
    ));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fixture.effects(), ["first", "second", "third"]);
    assert_eq!(session.inspect().unwrap().pid, pid);
}

#[test]
fn a_newer_owner_claim_fences_the_old_adapter_before_any_effect() {
    let fixture = Fixture::new();
    let old = adapter(&fixture.root);
    let new = adapter(&fixture.root);
    assert!(matches!(
        old.send_line("stale"),
        Err(Error::Unavailable(Unavailable::Recovering))
    ));
    new.send_line("current").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fixture.effects(), ["current"]);
    // Resize and close are not reserved, so the fence is checked up front.
    for refused in [old.resize(Size::new(100, 30)), old.terminate()] {
        assert!(matches!(
            refused,
            Err(Error::Unavailable(Unavailable::Recovering))
        ));
    }
    let observed = new.inspect().unwrap();
    assert_eq!(observed.lifecycle, Lifecycle::Running);
    assert_eq!(observed.size, Size::new(80, 24));
}

/// The store check and the host lease are not atomic; the host closes that gap.
#[test]
fn the_host_refuses_an_older_writer_that_already_passed_the_store_check() {
    let fixture = Fixture::new();
    let old = adapter(&fixture.root);
    let new = adapter(&fixture.root);
    assert!(new.owner.writer().epoch > old.owner.writer().epoch);
    new.resize(Size::for_pane(100, 30)).unwrap();
    // As if `old` had passed `current()` just before `new` claimed.
    let mut client =
        Client::open_expected(&old.endpoint, Role::Control, old.identity.clone()).unwrap();
    match client.acquire(old.owner.writer()).unwrap() {
        Response::Error { code } => assert_eq!(code, "superseded_writer"),
        _ => panic!("the host granted a superseded writer"),
    }
    let size = new.inspect().unwrap().size;
    assert_eq!((size.cols(), size.rows()), (100, 30));
}

#[test]
fn plain_text_skips_wide_continuations_and_joins_wrapped_rows() {
    use remuda_core::agent::{Cursor, StyledCell};
    let cell = |text: &str, wide: bool| StyledCell {
        text: text.into(),
        wide,
        ..StyledCell::default()
    };
    let screen = ScreenSnapshot {
        cells: vec![
            vec![cell("a", false), cell("", false), cell("b", false)],
            vec![cell("界", true), cell("", false), cell("c", false)],
            vec![cell("d", false), cell("", false), cell("", false)],
            vec![cell("", false), cell("", false), cell("", false)],
        ],
        wrapped: vec![false, true, false, false],
        cursor: Cursor {
            row: 0,
            col: 0,
            visible: true,
        },
        scrollback_len: 0,
        scrollback_total: 0,
    };
    assert_eq!(text(&screen), "a b\n界cd");
}

#[test]
fn controller_death_after_reservation_before_any_rpc_sends_the_original_once() {
    let fixture = Fixture::new();
    let mut old = helper(&fixture.root, "reserved");
    eventually("reservation cut", || {
        fixture.root.join("cut").exists().then_some(())
    });
    old.0.kill().unwrap();
    old.0.wait().unwrap();
    assert!(fixture.effects().is_empty());

    let session = adapter(&fixture.root);
    let Err(Error::Indeterminate(request)) = session.send_line("beta") else {
        panic!("the reserved act must be recovered first");
    };
    assert_eq!(
        request.payload,
        InputAct::SendLine("alpha".into()).digest().unwrap()
    );
    let recovered = eventually("reserved act completes", || {
        match session.recover(&request) {
            Ok(Recovery::Complete { delivery, reply }) => Some((delivery, reply)),
            Ok(Recovery::Pending) | Err(Error::Unavailable(_)) => None,
            other => panic!("unexpected recovery {other:?}"),
        }
    });
    assert!(matches!(
        recovered,
        (Delivery::Written, RecoveredReply::Unit)
    ));
    assert!(matches!(
        session.recover(&request).unwrap(),
        Recovery::Forgotten
    ));
    session.send_line("beta").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fixture.effects(), ["alpha", "beta"]);
}

#[test]
fn a_newer_claim_fences_resume_of_an_act_the_old_owner_already_reserved() {
    let fixture = Fixture::new();
    let old = adapter(&fixture.root);
    let act = InputAct::SendLine("stale".into());
    let key = old
        .owner
        .reserve(old.target(), [8; 16], CLIENT, 1, act)
        .unwrap();
    let _newer = adapter(&fixture.root);
    assert!(old.owner.resume(old.target(), key, &old.endpoint).is_err());
    std::thread::sleep(Duration::from_millis(200));
    assert!(fixture.effects().is_empty());
    // The current owner may still resolve that exact reserved act, once.
    let current = adapter(&fixture.root);
    let Err(Error::Indeterminate(request)) = current.send_line("next") else {
        panic!("the reserved act must be recovered first");
    };
    eventually("reserved act completes", || {
        match current.recover(&request) {
            Ok(Recovery::Complete { .. }) => Some(()),
            Ok(Recovery::Pending) | Err(Error::Unavailable(_)) => None,
            other => panic!("unexpected recovery {other:?}"),
        }
    });
    assert_eq!(fixture.effects(), ["stale"]);
}

#[test]
fn possibly_written_failures_stay_recoverable_and_retention_is_bounded() {
    let fixture = Fixture::new();
    let session = adapter(&fixture.root);
    // A refused clear key: the host reports every act error as Indeterminate.
    let requests: Vec<_> = (0..20)
        .map(|_| match session.clear_input(b"x") {
            Err(Error::Indeterminate(request)) => request,
            other => panic!("expected an unknown outcome, got {other:?}"),
        })
        .collect();
    let sequences: Vec<u64> = requests.iter().map(|request| request.sequence).collect();
    assert_eq!(sequences, (1..=20).collect::<Vec<_>>());
    let mut forgotten = 0;
    for request in &requests {
        match session.recover(request).unwrap() {
            Recovery::Forgotten => forgotten += 1,
            Recovery::Complete {
                delivery: Delivery::Indeterminate,
                reply: RecoveredReply::Failed(Error::Agent(AgentError::Io(_))),
            } => {}
            other => panic!("unexpected recovery {other:?}"),
        }
    }
    // Each reservation keeps at most RETAINED - 1 earlier open results.
    assert_eq!(forgotten, 20 - RETAINED);
    assert!(fixture.effects().is_empty());
}

#[test]
fn an_exited_session_refuses_new_input_before_any_reservation() {
    let fixture = Fixture::new();
    let session = adapter(&fixture.root);
    session.terminate().unwrap();
    eventually("child exit", || {
        matches!(session.inspect().ok()?.lifecycle, Lifecycle::Exited(_)).then_some(())
    });
    // Acts racing the exit publication may complete as possibly-written
    // failures; those stay recoverable evidence until resolved by key.
    eventually("exit published", || match session.send_line("late") {
        Err(Error::Agent(AgentError::Exited)) => Some(()),
        Err(Error::Indeterminate(request)) => {
            // Resolve that exact key before the next act; never a new sequence.
            eventually("late act resolves", || match session.recover(&request) {
                Ok(Recovery::Complete { delivery, .. }) => {
                    assert_eq!(delivery, Delivery::Indeterminate);
                    Some(())
                }
                Ok(Recovery::Forgotten) => Some(()),
                Ok(Recovery::Pending) | Err(Error::Unavailable(_)) => None,
                other => panic!("unexpected recovery {other:?}"),
            });
            None
        }
        // Nothing was reserved: a busy host may refuse the connection itself.
        Err(Error::Unavailable(_)) => None,
        other => panic!("unexpected input result {other:?}"),
    });
    let index = session.owner.index().unwrap();
    assert!(index.entries["synthetic"].pending.is_none());
    assert!(fixture.effects().is_empty());
}

#[test]
fn a_lease_held_longer_than_the_unavailability_cap_is_busy_not_absent() {
    let fixture = Fixture::new();
    let session = adapter(&fixture.root);
    // Another controller holds the lease for 6 s: the host answers Busy.
    let mut holder =
        Client::open_expected(&session.endpoint, Role::Control, session.identity.clone()).unwrap();
    assert!(matches!(
        holder.request(&Request::Acquire),
        Ok(Response::Granted { .. })
    ));
    let release = std::thread::spawn(move || {
        // Keep the connection alive past the host's idle deadline.
        for _ in 0..12 {
            std::thread::sleep(Duration::from_millis(500));
            let _ = holder.request(&Request::Inspect);
        }
    });
    session.send_line("late").unwrap();
    release.join().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fixture.effects(), ["late"]);
}

#[test]
fn a_new_daemon_resolves_an_old_act_by_key_before_new_input_without_replay() {
    let fixture = Fixture::new();
    let mut old = helper(&fixture.root, "controller");
    eventually("first effect", || {
        (fixture.effects().first().map(String::as_str) == Some("first")).then_some(())
    });
    old.0.kill().unwrap();
    old.0.wait().unwrap();
    // The host finishes the admitted act on its own.
    eventually("old act finished on the host", || {
        (fixture.effects() == ["first", "second"]).then_some(())
    });
    let session = adapter(&fixture.root);
    session.send_line("third").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fixture.effects(), ["first", "second", "third"]);
    // The old act's result is still retained for an explicit recovery.
    let old_act = RequestIdentity {
        session: session.identity(),
        client: CLIENT,
        sequence: 1,
        payload: session.owner.index().unwrap().entries["synthetic"].streams[0].results[0]
            .reservation
            .key
            .payload,
    };
    assert!(matches!(
        session.recover(&old_act).unwrap(),
        Recovery::Complete {
            delivery: Delivery::Written,
            ..
        }
    ));
}

#[test]
fn teardown_retires_an_entry_whose_host_was_stopped_before_a_crash() {
    let mut fixture = Fixture::new();
    let session = adapter(&fixture.root);
    session.terminate().unwrap();
    eventually("exit published", || {
        let index = session.owner.index().ok()?;
        matches!(index.entries["synthetic"].phase, Phase::Exited { .. }).then_some(())
    });
    // An old daemon stopped the host, then died before retiring the entry.
    eventually("host stopped", || {
        let mut client =
            Client::open_expected(&session.endpoint, Role::Control, session.identity.clone())
                .ok()?;
        matches!(
            client.request(&Request::Acquire),
            Ok(Response::Granted { .. })
        )
        .then_some(())?;
        matches!(client.request(&Request::Stop), Ok(Response::Ok)).then_some(())
    });
    let host = fixture.host.as_mut().unwrap();
    eventually("host exited", || host.0.try_wait().ok().flatten());
    // A new daemon finds no listener and still retires the settled entry.
    let fresh = adapter(&fixture.root);
    fresh.teardown(Duration::from_secs(10)).unwrap();
    assert!(!fresh
        .owner
        .index()
        .unwrap()
        .entries
        .contains_key("synthetic"));
}

#[test]
fn a_reclaim_fences_a_newer_claimant_at_the_store_and_at_the_host() {
    use crate::session_host::hosted::Hosting;
    let fixture = Fixture::new();
    let registry = remuda_core::Registry::new();
    let old = Hosting::at(fixture.root.clone());
    assert_eq!(old.discover(&registry).unwrap(), ["synthetic"]);
    let ours = registry.hosted("synthetic").unwrap();
    ours.send_line("old-1").unwrap();

    // A candidate claims: the old daemon is fenced.
    let candidate = adapter(&fixture.root);
    candidate.send_line("new-1").unwrap();
    assert!(matches!(
        ours.send_line("old-refused"),
        Err(Error::Unavailable(Unavailable::Recovering))
    ));

    // Rollback: the old daemon claims again and shows the host.
    assert_eq!(
        old.reclaim().unwrap(),
        [("synthetic".to_owned(), Announced::Fenced)]
    );
    // The host refuses the candidate's writer before the old daemon's next
    // act reaches it, store check or not.
    let mut raw = Client::open_expected(
        &candidate.endpoint,
        Role::Control,
        candidate.identity.clone(),
    )
    .unwrap();
    assert!(matches!(
        raw.acquire(candidate.owner.writer()).unwrap(),
        Response::Error { code } if code == "superseded_writer"
    ));
    drop(raw);
    ours.send_line("old-2").unwrap();
    assert!(matches!(
        candidate.send_line("new-refused"),
        Err(Error::Unavailable(Unavailable::Recovering))
    ));
    assert_eq!(fixture.effects(), ["old-1", "new-1", "old-2"]);
}

#[test]
fn a_preflight_reads_every_host_without_claiming_or_taking_the_lease() {
    use crate::session_host::hosted::{Checked, CheckedState, Hosting};
    let mut fixture = Fixture::new();
    let session = adapter(&fixture.root);
    let before = session.owner.index().unwrap().fence();

    let checked = Hosting::at(fixture.root.clone()).preflight().unwrap();
    assert_eq!(
        checked,
        [Checked {
            name: "synthetic".into(),
            state: CheckedState::Running {
                build: crate::dist::BUILD_VERSION.into()
            },
        }]
    );
    // No claim: the existing owner is still the writer and acts at once.
    assert_eq!(session.owner.index().unwrap().fence(), before);
    assert!(session.owner.current().unwrap());
    session.send_line("after-preflight").unwrap();
    assert_eq!(fixture.effects(), ["after-preflight"]);

    let host = fixture.host.as_mut().unwrap();
    host.0.kill().unwrap();
    host.0.wait().unwrap();
    let checked = Hosting::at(fixture.root.clone()).preflight().unwrap();
    assert!(
        matches!(
            &checked[..],
            [Checked {
                state: CheckedState::Unreachable(_),
                ..
            }]
        ),
        "{checked:?}"
    );
}

#[test]
fn a_drained_gate_refuses_acts_and_control_before_any_effect_but_reads_go_on() {
    let fixture = Fixture::new();
    let gate = Arc::new(crate::session_host::gate::Gate::default());
    let session = adapter(&fixture.root).gated(Arc::clone(&gate));
    session.send_line("before").unwrap();
    assert_eq!(fixture.effects(), ["before"]);
    let size = session.inspect().unwrap().size;

    assert_eq!(gate.drain(Duration::from_secs(1)).left, 0);
    let refused = |result: remuda_core::session_operations::Result<()>| {
        matches!(result, Err(Error::Unavailable(Unavailable::Recovering)))
    };
    assert!(refused(session.send_line("after")));
    assert!(refused(session.resize(Size::for_pane(100, 30))));
    assert!(refused(session.terminate()));
    // Nothing reached the host: no effect, no resize, no reservation.
    assert_eq!(fixture.effects(), ["before"]);
    let seen = session.inspect().unwrap();
    assert_eq!(seen.lifecycle, Lifecycle::Running);
    assert_eq!(seen.size, size);
    assert!(session.screen_text().unwrap().contains("effect:before"));
    let index = session.owner.index().unwrap();
    assert_eq!(index.entries["synthetic"].streams[0].next, Some(2));
}

#[test]
fn teardown_leaves_an_in_flight_act_its_own_result() {
    let mut fixture = Fixture::new();
    let session = Arc::new(adapter(&fixture.root));
    // EOF ends the child's read loop while the act is still in its pause.
    let acting = {
        let session = Arc::clone(&session);
        std::thread::spawn(move || session.feed(&[Step::Burst(vec![4]), Step::Pause(600)]))
    };
    eventually("exit published", || {
        let index = session.owner.index().ok()?;
        matches!(index.entries["synthetic"].phase, Phase::Exited { .. }).then_some(())
    });
    session.teardown(Duration::from_secs(10)).unwrap();
    let result = acting.join().unwrap();
    assert!(result.is_ok(), "teardown took the act's result: {result:?}");
    let host = fixture.host.as_mut().unwrap();
    eventually("host stopped", || host.0.try_wait().ok().flatten());
}

#[test]
fn teardown_waits_while_an_act_is_in_flight() {
    let mut fixture = Fixture::new();
    let session = Arc::new(adapter(&fixture.root));
    session.terminate().unwrap();
    eventually("exit published", || {
        let index = session.owner.index().ok()?;
        matches!(index.entries["synthetic"].phase, Phase::Exited { .. }).then_some(())
    });
    // An act in flight holds this lock until it has settled its own result.
    let in_flight = session.acts.lock().unwrap();
    let tearing = {
        let session = Arc::clone(&session);
        std::thread::spawn(move || session.teardown(Duration::from_secs(10)))
    };
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !tearing.is_finished(),
        "teardown ran under an act in flight"
    );
    assert!(session
        .owner
        .index()
        .unwrap()
        .entries
        .contains_key("synthetic"));
    drop(in_flight);
    tearing.join().unwrap().unwrap();
    let host = fixture.host.as_mut().unwrap();
    eventually("host stopped", || host.0.try_wait().ok().flatten());
}
