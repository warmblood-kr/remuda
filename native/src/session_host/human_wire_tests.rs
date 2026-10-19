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

#[test]
fn human_role_needs_negotiated_capability_and_view_cannot_grant() {
    let fixture = Fixture::new();
    assert!(
        Client::open_human_without_capability(&fixture.path(), fixture.host.identity()).is_err()
    );
    let mut view =
        Client::open_expected(&fixture.path(), Role::View, fixture.host.identity()).unwrap();
    assert_eq!(code(grant(&mut view, 1, 0, 1)), "human_role_required");
    assert_eq!(
        code(view.request(&Request::RawCut { last: 1 }).unwrap()),
        "raw_outcome_not_visible"
    );
    let mut control = fixture.control();
    assert_eq!(code(grant(&mut control, 1, 0, 1)), "human_role_required");
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
