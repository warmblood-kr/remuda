//! Synthetic PTY races and raw-input backpressure (#713). Coordinator races
//! release contenders together on a Barrier; wire tests count actual effects
//! from a /bin/sh child that leaves stdin unread until GATE exists, so one
//! write stays physically in flight. A final sentinel line proves no replay:
//! the PTY is FIFO, so any replay would land before it.
use super::super::human::MAX_RAW as MAX;
use super::*;
use crate::session_host::api::{Info, Lifecycle, Request, Response, Role};
use crate::session_host::control::Refusal;
use crate::session_host::entry::Client;
use crate::session_host::server::Server;
use crate::session_host::unix::Endpoint;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Barrier;
use std::time::Instant;

const ITERATIONS: usize = 500;
const BIG: usize = super::super::wire::MAX_INPUT;

fn request(id: u8, expected: u64) -> GrantRequest {
    GrantRequest {
        request: [id; 16],
        expected,
        recovery: [id; 32],
    }
}
/// Skew rotates which side starts late, so every interleaving order is
/// exercised: round 0 none, round 1 the first contender, round 2 the others.
fn skew(round: usize, first: bool) -> u32 {
    match (round % 3, first) {
        (1, true) | (2, false) => 20_000,
        _ => 0,
    }
}
fn race<T: Send + 'static>(
    barrier: &Arc<Barrier>,
    spins: u32,
    body: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    let barrier = barrier.clone();
    std::thread::spawn(move || {
        barrier.wait();
        for _ in 0..spins {
            std::hint::spin_loop();
        }
        body()
    })
}

#[test]
fn stop_latch_racing_controller_and_human_admission_admits_nothing_after_ok() {
    let (mut latched, mut refused) = (0, 0);
    for round in 0..ITERATIONS {
        let mut host = Coordinator::new([1; 16]);
        let owner = host.acquire(ConnectionId(1)).unwrap();
        let host = Arc::new(Mutex::new(host));
        let barrier = Arc::new(Barrier::new(3));
        let act = InputAct::Bytes(b"x".to_vec());
        let stop = {
            let host = host.clone();
            race(&barrier, skew(round, true), move || {
                host.lock().unwrap().latch_stop(owner)
            })
        };
        let controller = {
            let (host, act) = (host.clone(), act.clone());
            race(&barrier, skew(round, false), move || {
                matches!(
                    host.lock().unwrap().admit(owner, [1; 16], 1, &act),
                    Ok(Admission::Started(_))
                )
            })
        };
        let human = {
            let host = host.clone();
            race(&barrier, skew(round, false), move || {
                let human = ConnectionId(9);
                let granted = host.lock().unwrap().grant(human, request(1, 0));
                match granted {
                    Ok((grant, _)) => {
                        let raw = host.lock().unwrap().admit_raw(human, grant, 1, [7; 32]);
                        (true, raw == Ok(Raw::New))
                    }
                    Err(error) => {
                        assert_eq!(error, Error::Stopping);
                        (false, false)
                    }
                }
            })
        };
        let stop = stop.join().unwrap();
        let started = controller.join().unwrap();
        let (granted, written) = human.join().unwrap();
        // One physical barrier: never a controller act and a raw ticket.
        assert!(!(started && written), "two writers admitted");
        let mut host = host.lock().unwrap();
        match stop {
            Ok(()) => {
                latched += 1;
                assert!(!started && !granted && !written, "admitted after Stop Ok");
                assert_eq!(host.admit(owner, [1; 16], 2, &act), Err(Error::Stopping));
                assert_eq!(
                    host.grant(ConnectionId(9), request(2, 0)),
                    Err(Error::Stopping)
                );
                assert_eq!(host.begin_mutation(owner), Err(Error::Stopping));
                // A retried Stop after a lost reply can never re-admit.
                assert_eq!(host.latch_stop(owner), Err(Error::Stopping));
                assert_eq!(host.recover_raw(1, 1, [7; 32]), Raw::OutOfOrder);
            }
            Err(Error::Control(Refusal::Busy)) => {
                refused += 1;
                assert!(started || written, "Busy without an admitted write");
            }
            Err(Error::Retained) => {
                refused += 1;
                assert!(granted, "Retained without a live grant");
            }
            Err(other) => panic!("unexpected Stop refusal {other:?}"),
        }
    }
    eprintln!("stop race: latched {latched}, refused {refused} of {ITERATIONS}");
}

#[test]
fn takeover_and_controller_mutation_never_overlap_an_in_flight_raw_ticket() {
    let (mut handoffs, mut busy) = (0, 0);
    for round in 0..ITERATIONS {
        let mut host = Coordinator::new([2; 16]);
        let owner = host.acquire(ConnectionId(1)).unwrap();
        let (old, new) = (ConnectionId(9), ConnectionId(10));
        assert_eq!(host.grant(old, request(1, 0)), Ok((1, GrantState::Current)));
        assert_eq!(host.admit_raw(old, 1, 1, [1; 32]), Ok(Raw::New));
        let host = Arc::new(Mutex::new(host));
        // Writers currently holding the physical barrier; the old ticket is one.
        let active = Arc::new(AtomicUsize::new(1));
        let barrier = Arc::new(Barrier::new(3));
        let completer = {
            let (host, active) = (host.clone(), active.clone());
            race(&barrier, skew(round, true), move || {
                active.fetch_sub(1, Ordering::SeqCst);
                host.lock().unwrap().complete_raw(1, Delivery::Written)
            })
        };
        let takeover = {
            let (host, active) = (host.clone(), active.clone());
            race(&barrier, skew(round, false), move || {
                let granted = host.lock().unwrap().grant(new, request(2, 1));
                assert_eq!(granted, Ok((2, GrantState::Current)), "mid-write takeover");
                let raw = host.lock().unwrap().admit_raw(new, 2, 2, [2; 32]).unwrap();
                if raw == Raw::New {
                    assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0, "second writer");
                    active.fetch_sub(1, Ordering::SeqCst);
                    host.lock()
                        .unwrap()
                        .complete_raw(2, Delivery::Written)
                        .unwrap();
                }
                raw
            })
        };
        let mutation = {
            let (host, active) = (host.clone(), active.clone());
            race(&barrier, skew(round, false), move || {
                let token = host.lock().unwrap().begin_mutation(owner);
                match token {
                    Ok(token) => {
                        assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0, "resize overlap");
                        active.fetch_sub(1, Ordering::SeqCst);
                        host.lock().unwrap().finish_mutation(token).unwrap();
                    }
                    Err(error) => assert_eq!(error, Error::Control(Refusal::Busy)),
                }
            })
        };
        assert_eq!(completer.join().unwrap(), Ok(()));
        let raw = takeover.join().unwrap();
        mutation.join().unwrap();
        let mut host = host.lock().unwrap();
        // The old ticket completed exactly once, by sequence, not by holder.
        assert_eq!(
            host.complete_raw(1, Delivery::Indeterminate),
            Err(Error::StaleCompletion)
        );
        assert_eq!(
            host.recover_raw(1, 1, [1; 32]),
            Raw::Complete(Delivery::Written)
        );
        match raw {
            Raw::New => handoffs += 1,
            Raw::Busy => {
                busy += 1;
                // Busy spent no sequence: the same sequence is New now.
                assert_eq!(host.admit_raw(new, 2, 2, [2; 32]), Ok(Raw::New));
            }
            other => panic!("unexpected takeover admission {other:?}"),
        }
        assert_eq!(host.admit_raw(old, 1, 3, [3; 32]), Ok(Raw::Stale));
    }
    eprintln!("takeover race: handed off {handoffs}, busy {busy} of {ITERATIONS}");
}

#[test]
fn raw_capacity_racing_its_exact_ack_spends_nothing_and_evicts_nothing() {
    let (mut first, mut later) = (0, 0);
    let full = MAX as u64;
    for round in 0..ITERATIONS {
        let mut host = Coordinator::new([3; 16]);
        let human = ConnectionId(9);
        host.grant(human, request(1, 0)).unwrap();
        for sequence in 1..=full {
            assert_eq!(host.admit_raw(human, 1, sequence, [0; 32]), Ok(Raw::New));
            host.complete_raw(sequence, Delivery::Written).unwrap();
        }
        assert_eq!(host.admit_raw(human, 1, full + 1, [0; 32]), Ok(Raw::Busy));
        assert_eq!(host.recover_raw(1, full + 1, [0; 32]), Raw::OutOfOrder);
        assert_eq!(
            host.recover_raw(1, 1, [0; 32]),
            Raw::Complete(Delivery::Written)
        );
        let cut = host.raw_cut(1).unwrap();
        let host = Arc::new(Mutex::new(host));
        let barrier = Arc::new(Barrier::new(2));
        let ack = {
            let host = host.clone();
            race(&barrier, skew(round, true), move || {
                host.lock().unwrap().ack_raw(human, cut)
            })
        };
        let admit = {
            let host = host.clone();
            race(&barrier, skew(round, false), move || {
                host.lock().unwrap().admit_raw(human, 1, full + 1, [0; 32])
            })
        };
        assert_eq!(ack.join().unwrap(), Retire::Retired);
        let admitted = admit.join().unwrap();
        let mut host = host.lock().unwrap();
        match admitted {
            Ok(Raw::New) => first += 1,
            Ok(Raw::Busy) => {
                later += 1;
                assert_eq!(host.admit_raw(human, 1, full + 1, [0; 32]), Ok(Raw::New));
            }
            other => panic!("unexpected capacity admission {other:?}"),
        }
        assert_eq!(host.recover_raw(1, 1, [0; 32]), Raw::Forgotten);
        assert_eq!(
            host.recover_raw(1, 2, [0; 32]),
            Raw::Complete(Delivery::Written)
        );
        host.complete_raw(full + 1, Delivery::Written).unwrap();
        // The exact one-entry cut freed exactly one slot.
        assert_eq!(host.admit_raw(human, 1, full + 2, [0; 32]), Ok(Raw::Busy));
    }
    eprintln!("capacity race: admitted first {first}, after ack {later} of {ITERATIONS}");
}

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    host: Arc<Owned>,
    server: Arc<Server>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    /// The child switches to raw mode, then reads nothing until GATE exists.
    fn new(gated: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "rm-race-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        if !gated {
            std::fs::write(root.join("gate"), b"").unwrap();
        }
        let endpoint = Endpoint::bind(&root.join("host")).unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            "stty raw -echo; printf 'race-ready\\n'; \
             while [ ! -e \"$GATE\" ]; do sleep 0.01; done; \
             while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$EFFECTS\"; done",
        ]);
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("EFFECTS", root.join("effects"));
        command.env("GATE", root.join("gate"));
        command.cwd(&root);
        let host =
            Arc::new(Owned::spawn([5; 16], "race".into(), command, Size::new(80, 24)).unwrap());
        eventually("child ready", || {
            host.session.screen_text().unwrap().contains("race-ready")
        });
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
    fn open_gate(&self) {
        std::fs::write(self.root.join("gate"), b"").unwrap();
    }
    fn path(&self) -> PathBuf {
        self.root.join("host")
    }
    fn human(&self) -> Client {
        Client::open_human(&self.path(), self.host.identity()).unwrap()
    }
    /// A replacement controller waits for a dropped one's fence to release.
    fn control(&self) -> Client {
        let mut client =
            Client::open_expected(&self.path(), Role::Control, self.host.identity()).unwrap();
        let until = Instant::now() + Duration::from_secs(4);
        loop {
            match client.request(&Request::Acquire).unwrap() {
                Response::Granted { .. } => return client,
                Response::Error { code } if code == "controller_busy" => {}
                other => panic!("{other:?}"),
            }
            assert!(Instant::now() < until, "controller never released");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn effects(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("effects"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
    /// Write a sentinel as the newest effect and wait for it; anything a
    /// replay could add must already precede it.
    fn effects_through(&self, sentinel: &[u8]) -> Vec<String> {
        let sentinel = text(sentinel);
        eventually("sentinel effect", || {
            self.effects().last() == Some(&sentinel)
        });
        let mut effects = self.effects();
        effects.pop();
        effects
    }
    fn writer_busy(&self) -> bool {
        self.host.writer.is_busy()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.open_gate();
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            match self.host.acquire(ConnectionId(u128::MAX)) {
                Ok(fence) => {
                    let result = self.host.close(fence);
                    let _ = self.host.release(fence);
                    if result.is_ok() {
                        break;
                    }
                }
                // Stop latched only after a confirmed exit.
                Err(Error::Stopping) => break,
                Err(_) => {}
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
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(4);
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}
/// One effect line: `<seq>:xxx…\n`, exactly `len` bytes.
fn line(seq: u64, len: usize) -> Vec<u8> {
    let mut bytes = format!("{seq}:").into_bytes();
    bytes.resize(len - 1, b'x');
    bytes.push(b'\n');
    bytes
}
fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.strip_suffix(b"\n").unwrap().to_vec()).unwrap()
}
fn grant(client: &mut Client, id: u64, expected: u64) -> Response {
    client
        .request(&Request::HumanGrant {
            request: u128::from(id).to_be_bytes(),
            expected,
            recovery_secret: [id as u8; 32],
        })
        .unwrap()
}
fn granted(response: Response) -> (u64, GrantState) {
    match response {
        Response::HumanGranted { grant, state } => (grant, state),
        other => panic!("expected grant, got {other:?}"),
    }
}
fn code(response: Response) -> String {
    match response {
        Response::Error { code } => code,
        other => panic!("expected refusal, got {other:?}"),
    }
}
fn ok(response: Response) {
    assert!(matches!(response, Response::Ok), "{response:?}");
}
fn raw(response: Response) -> Raw {
    match response {
        Response::RawOutcome { state } => state,
        other => panic!("expected raw outcome, got {other:?}"),
    }
}
fn input(client: &mut Client, grant: u64, sequence: u64, bytes: &[u8]) -> Raw {
    raw(client
        .request(&Request::HumanInput {
            grant,
            sequence,
            bytes: bytes.to_vec(),
        })
        .unwrap())
}
fn recover(client: &mut Client, grant: u64, sequence: u64, bytes: &[u8]) -> Raw {
    raw(client
        .request(&Request::RecoverRaw {
            grant,
            sequence,
            bytes: bytes.to_vec(),
        })
        .unwrap())
}
fn settled(client: &mut Client, grant: u64, sequence: u64, bytes: &[u8]) -> Raw {
    let mut state = Raw::Pending;
    eventually("raw settlement", || {
        state = recover(client, grant, sequence, bytes);
        state != Raw::Pending
    });
    state
}
fn write(client: &mut Client, grant: u64, sequence: u64, bytes: &[u8]) {
    assert_eq!(input(client, grant, sequence, bytes), Raw::New);
    assert_eq!(
        settled(client, grant, sequence, bytes),
        Raw::Complete(Delivery::Written)
    );
}
fn cut(client: &mut Client, last: u64) -> Cut {
    match client.request(&Request::RawCut { last }).unwrap() {
        Response::RawCut { cut } => cut,
        other => panic!("expected cut, got {other:?}"),
    }
}
fn retired(response: Response) -> Retire {
    match response {
        Response::Retired { state } => state,
        other => panic!("expected retirement, got {other:?}"),
    }
}
fn info(client: &mut Client) -> Info {
    match client.request(&Request::Inspect).unwrap() {
        Response::Info { info } => info,
        other => panic!("expected info, got {other:?}"),
    }
}
/// Full-size lines until one stays physically blocked on the gated child;
/// returns that in-flight sequence. Earlier lines fill the PTY buffer.
fn block(fixture: &Fixture, client: &mut Client, grant: u64, first: u64) -> u64 {
    for sequence in first..first + 64 {
        let bytes = line(sequence, BIG);
        assert_eq!(input(client, grant, sequence, &bytes), Raw::New);
        let until = Instant::now() + Duration::from_millis(300);
        while recover(client, grant, sequence, &bytes) == Raw::Pending {
            if Instant::now() >= until && fixture.writer_busy() {
                eprintln!("PTY write blocked at sequence {sequence}");
                return sequence;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            recover(client, grant, sequence, &bytes),
            Raw::Complete(Delivery::Written)
        );
    }
    panic!("gated child never blocked a PTY write");
}
fn lines(range: std::ops::RangeInclusive<u64>, len: usize) -> Vec<String> {
    range.map(|seq| text(&line(seq, len))).collect()
}

#[test]
fn lost_stop_reply_then_reconnect_and_retry_never_admits_or_replays() {
    let fixture = Fixture::new(false);
    let mut human = fixture.human();
    let (grant, _) = granted(grant(&mut human, 1, 0));
    let one = line(1, 16);
    write(&mut human, grant, 1, &one);
    let mut control = fixture.control();
    assert_eq!(
        code(control.request(&Request::Stop).unwrap()),
        "host_stop_requires_confirmed_exit_and_output_eof"
    );
    assert!(matches!(
        control.request(&Request::RevokeHuman { grant }).unwrap(),
        Response::Revoked { revoked: true }
    ));
    eventually("effect before close", || {
        fixture.effects() == lines(1..=1, 16)
    });
    ok(control.request(&Request::Close).unwrap());
    eventually("confirmed exit", || {
        let info = info(&mut control);
        matches!(info.lifecycle, Lifecycle::Exited) && info.output_eof
    });
    // Lost reply to a refused Stop: the retry answers the same refusal.
    control.send_fixture(&Request::Stop).unwrap();
    drop(control);
    let mut control = fixture.control();
    assert_eq!(
        code(control.request(&Request::Stop).unwrap()),
        "host_raw_retention_pending"
    );
    assert_eq!(
        recover(&mut control, grant, 1, &one),
        Raw::Complete(Delivery::Written)
    );
    let cut = cut(&mut control, 1);
    assert_eq!(
        retired(control.request(&Request::DiscardRaw { cut }).unwrap()),
        Retire::Retired
    );
    // Lost reply to an accepted Stop: the latch is final and admits nothing.
    control.send_fixture(&Request::Stop).unwrap();
    drop(control);
    eventually("stop latch", || {
        matches!(
            fixture.host.acquire(ConnectionId(u128::MAX)),
            Err(Error::Stopping)
        )
    });
    if let Ok(mut retry) =
        Client::open_expected(&fixture.path(), Role::Control, fixture.host.identity())
    {
        assert_eq!(
            code(retry.request(&Request::Acquire).unwrap()),
            "host_stopping"
        );
        assert_eq!(
            code(retry.request(&Request::Stop).unwrap()),
            "controller_required"
        );
    }
    assert_eq!(
        fixture.host.grant_human(ConnectionId(9), request(2, 1)),
        Err(Error::Stopping)
    );
    assert_eq!(
        fixture
            .host
            .admit_human_raw(ConnectionId(9), grant, 2, line(2, 16)),
        Err(Error::Stopping)
    );
    assert_eq!(
        fixture
            .host
            .recover_raw(grant, 1, InputAct::Bytes(one.clone()).digest().unwrap()),
        Raw::Forgotten
    );
    assert_eq!(fixture.effects(), lines(1..=1, 16));
}

#[test]
fn wire_stop_and_human_grant_race_has_exactly_one_winner() {
    let (mut stopped, mut grants) = (0, 0);
    for round in 0..9 {
        let fixture = Fixture::new(false);
        // Wire latency dwarfs spins; skew by a short sleep instead.
        let late = |first| Duration::from_millis(u64::from(skew(round, first) / 1_000));
        let mut control = fixture.control();
        ok(control.request(&Request::Close).unwrap());
        eventually("confirmed exit", || {
            let info = info(&mut control);
            matches!(info.lifecycle, Lifecycle::Exited) && info.output_eof
        });
        let mut human = fixture.human();
        let barrier = Arc::new(Barrier::new(2));
        let delay = late(true);
        let stop = race(&barrier, 0, move || {
            std::thread::sleep(delay);
            let response = control.request(&Request::Stop).unwrap();
            (response, control)
        });
        let delay = late(false);
        let take = race(&barrier, 0, move || {
            std::thread::sleep(delay);
            let response = grant(&mut human, 1, 0);
            (response, human)
        });
        let (stop, _control) = stop.join().unwrap();
        let (taken, _human) = take.join().unwrap();
        match (stop, taken) {
            (Response::Ok, Response::Error { code }) => {
                assert_eq!(code, "host_stopping");
                assert!(!fixture.host.human_attached_fixture());
                stopped += 1;
            }
            (Response::Error { code }, Response::HumanGranted { .. }) => {
                assert_eq!(code, "invalid_or_unavailable_operation", "human attached");
                assert!(fixture.host.human_attached_fixture());
                grants += 1;
            }
            other => panic!("Stop and grant must have exactly one winner: {other:?}"),
        }
        assert!(fixture.effects().is_empty());
    }
    eprintln!("wire stop race: stopped {stopped}, granted {grants}");
}

#[test]
fn takeover_mid_write_gets_busy_and_old_ticket_completes_exactly_once() {
    let fixture = Fixture::new(true);
    let mut old = fixture.human();
    let mut new = fixture.human();
    let mut control = fixture.control();
    let (first, _) = granted(grant(&mut old, 1, 0));
    let flight = block(&fixture, &mut old, first, 1);
    let flying = line(flight, BIG);
    // Takeover is admitted mid-write; the new holder cannot start a writer.
    let (second, state) = granted(grant(&mut new, 2, first));
    assert_eq!(state, GrantState::Current);
    let next = line(flight + 1, 16);
    for _ in 0..5 {
        assert_eq!(input(&mut new, second, flight + 1, &next), Raw::Busy);
    }
    assert_eq!(input(&mut old, first, flight + 1, &next), Raw::Stale);
    assert_eq!(recover(&mut control, first, flight, &flying), Raw::Pending);
    assert!(fixture.writer_busy(), "old ticket drains on the PTY");
    fixture.open_gate();
    let done = settled(&mut control, first, flight, &flying);
    assert!(
        matches!(
            done,
            Raw::Complete(Delivery::Written | Delivery::Indeterminate)
        ),
        "{done:?}"
    );
    // Busy spent nothing: the same sequence is New once the ticket drains.
    write(&mut new, second, flight + 1, &next);
    assert_eq!(recover(&mut control, first, flight, &flying), done);
    let sentinel = line(flight + 2, 16);
    write(&mut new, second, flight + 2, &sentinel);
    let mut expected = lines(1..=flight, BIG);
    expected.push(text(&next));
    assert_eq!(fixture.effects_through(&sentinel), expected);
}

#[test]
fn disconnect_with_a_late_writer_retains_a_recoverable_outcome_without_replay() {
    let fixture = Fixture::new(true);
    let mut control = fixture.control();
    let mut human = fixture.human();
    let (first, _) = granted(grant(&mut human, 1, 0));
    let flight = block(&fixture, &mut human, first, 1);
    let flying = line(flight, BIG);
    drop(human);
    let mut back = fixture.human();
    // A lost-ACK retry after disconnect is evidence only, never a transfer.
    eventually("disconnect releases the grant", || {
        granted(grant(&mut back, 1, 0)) == (first, GrantState::Released)
    });
    assert!(!fixture.host.holds_grant(ConnectionId(0), first));
    assert_eq!(recover(&mut control, first, flight, &flying), Raw::Pending);
    assert!(fixture.writer_busy(), "late writer still on the PTY");
    assert_eq!(input(&mut back, first, flight, &flying), Raw::Pending);
    fixture.open_gate();
    let done = settled(&mut control, first, flight, &flying);
    assert_eq!(done, Raw::Complete(Delivery::Written));
    // Retained retries answer before the grant check and never replay.
    assert_eq!(input(&mut back, first, flight, &flying), done);
    assert_eq!(
        input(&mut back, first, flight + 1, &line(flight + 1, 16)),
        Raw::Stale
    );
    let (second, _) = granted(grant(&mut back, 2, first));
    let cut = cut(&mut control, flight);
    assert_eq!(cut.grant, first);
    assert_eq!(
        retired(control.request(&Request::DiscardRaw { cut }).unwrap()),
        Retire::Retired
    );
    assert_eq!(
        recover(&mut control, first, flight, &flying),
        Raw::Forgotten
    );
    let sentinel = line(flight + 1, 16);
    write(&mut back, second, flight + 1, &sentinel);
    assert_eq!(fixture.effects_through(&sentinel), lines(1..=flight, BIG));
}

#[test]
fn raw_cap_is_busy_without_spending_or_evicting_until_the_exact_cut_is_acked() {
    let fixture = Fixture::new(false);
    let mut human = fixture.human();
    let (grant, _) = granted(grant(&mut human, 1, 0));
    let full = MAX as u64;
    for sequence in 1..=full {
        write(&mut human, grant, sequence, &line(sequence, 16));
    }
    let over = line(full + 1, 16);
    for _ in 0..3 {
        assert_eq!(input(&mut human, grant, full + 1, &over), Raw::Busy);
    }
    assert_eq!(recover(&mut human, grant, full + 1, &over), Raw::OutOfOrder);
    assert_eq!(
        input(&mut human, grant, full + 2, &line(full + 2, 16)),
        Raw::OutOfOrder,
        "Busy advanced the sequence"
    );
    assert_eq!(
        recover(&mut human, grant, 1, &line(1, 16)),
        Raw::Complete(Delivery::Written),
        "capacity evicted the oldest outcome"
    );
    let exact = cut(&mut human, 1);
    let forged = Cut {
        hash: [0; 32],
        ..exact
    };
    let ack = |client: &mut Client, cut: Cut| {
        retired(client.request(&Request::AcknowledgeRaw { cut }).unwrap())
    };
    assert_eq!(ack(&mut human, forged), Retire::Conflict);
    assert_eq!(input(&mut human, grant, full + 1, &over), Raw::Busy);
    assert_eq!(ack(&mut human, exact), Retire::Retired);
    assert_eq!(ack(&mut human, exact), Retire::Retired, "exact retry");
    write(&mut human, grant, full + 1, &over);
    assert_eq!(recover(&mut human, grant, 1, &line(1, 16)), Raw::Forgotten);
    let sentinel = line(full + 2, 16);
    assert_eq!(input(&mut human, grant, full + 2, &sentinel), Raw::Busy);
    let two = cut(&mut human, 2);
    assert_eq!(ack(&mut human, two), Retire::Retired);
    write(&mut human, grant, full + 2, &sentinel);
    assert_eq!(fixture.effects_through(&sentinel), lines(1..=full + 1, 16));
}

#[test]
fn controller_resize_close_and_input_contend_with_a_human_ticket() {
    let fixture = Fixture::new(true);
    let mut control = fixture.control();
    let mut human = fixture.human();
    let (grant, _) = granted(grant(&mut human, 1, 0));
    let flight = block(&fixture, &mut human, grant, 1);
    let resize = Request::Resize {
        rows: 30,
        cols: 100,
    };
    let act = InputAct::Bytes(b"controller\n".to_vec());
    for _ in 0..3 {
        assert_eq!(code(control.request(&resize).unwrap()), "controller_busy");
        assert_eq!(
            code(control.request(&Request::Close).unwrap()),
            "controller_busy"
        );
        assert_eq!(
            code(
                control
                    .request(&Request::Input {
                        client: [1; 16],
                        sequence: 1,
                        act: act.clone(),
                    })
                    .unwrap()
            ),
            "controller_busy"
        );
    }
    assert_eq!((info(&mut control).rows, info(&mut control).cols), (24, 80));
    assert!(fixture.host.session.is_alive(), "close refused mid-write");
    fixture.open_gate();
    assert_eq!(
        settled(&mut human, grant, flight, &line(flight, BIG)),
        Raw::Complete(Delivery::Written)
    );
    ok(control.request(&resize).unwrap());
    assert_eq!(
        (info(&mut control).rows, info(&mut control).cols),
        (30, 100)
    );
    let sentinel = line(flight + 1, 16);
    write(&mut human, grant, flight + 1, &sentinel);
    assert_eq!(fixture.effects_through(&sentinel), lines(1..=flight, BIG));
}

#[test]
fn in_flight_controller_act_makes_human_input_busy_without_spending_a_sequence() {
    let fixture = Fixture::new(true);
    let mut control = fixture.control();
    let mut flight = 0;
    for sequence in 1..64 {
        let act = InputAct::Bytes(line(sequence, BIG));
        assert!(matches!(
            control
                .request(&Request::Input {
                    client: [1; 16],
                    sequence,
                    act: act.clone(),
                })
                .unwrap(),
            Response::Admitted
        ));
        let pending = |control: &mut Client| {
            matches!(
                control
                    .request(&Request::Recover {
                        client: [1; 16],
                        sequence,
                        act: act.clone(),
                    })
                    .unwrap(),
                Response::Outcome {
                    state: super::super::api::OutcomeState::Pending,
                    ..
                }
            )
        };
        let until = Instant::now() + Duration::from_millis(300);
        while pending(&mut control) && !(Instant::now() >= until && fixture.writer_busy()) {
            std::thread::sleep(Duration::from_millis(5));
        }
        if pending(&mut control) {
            flight = sequence;
            break;
        }
    }
    assert!(flight > 0, "gated child never blocked a controller act");
    let mut human = fixture.human();
    let (grant, _) = granted(grant(&mut human, 1, 0));
    let mine = line(1_000, 16);
    for _ in 0..5 {
        assert_eq!(input(&mut human, grant, 1, &mine), Raw::Busy);
    }
    assert_eq!(recover(&mut human, grant, 1, &mine), Raw::OutOfOrder);
    fixture.open_gate();
    eventually("controller act drains", || {
        input(&mut human, grant, 1, &mine) != Raw::Busy
    });
    assert_eq!(
        settled(&mut human, grant, 1, &mine),
        Raw::Complete(Delivery::Written)
    );
    let sentinel = line(1_001, 16);
    write(&mut human, grant, 2, &sentinel);
    let mut expected = lines(1..=flight, BIG);
    expected.push(text(&mine));
    assert_eq!(fixture.effects_through(&sentinel), expected);
}

#[test]
fn reconnect_and_grant_churn_keeps_per_client_state_bounded() {
    // More rounds than MAX_CONNECTIONS and MAX_RAW: a leak would refuse.
    const ROUNDS: u64 = 300;
    let fixture = Fixture::new(false);
    let mut displaced: Option<Client> = None;
    for round in 1..=ROUNDS {
        let mut control = fixture.control();
        let mut human = fixture.human();
        let (grant, state) = granted(grant(&mut human, round, round - 1));
        assert_eq!((grant, state), (round, GrantState::Current));
        // Odd rounds are displaced by the next grant before they disconnect.
        drop(displaced.take());
        let bytes = line(round, 16);
        write(&mut human, grant, round, &bytes);
        let cut = cut(&mut human, round);
        assert_eq!(
            retired(human.request(&Request::AcknowledgeRaw { cut }).unwrap()),
            Retire::Retired
        );
        ok(control.request(&Request::Release).unwrap());
        drop(control);
        if round % 2 == 1 {
            displaced = Some(human);
        }
    }
    drop(displaced);
    let mut view =
        Client::open_expected(&fixture.path(), Role::View, fixture.host.identity()).unwrap();
    eventually("connections drain", || {
        info(&mut view).active_connections == 1
    });
    assert!(!fixture.host.human_attached_fixture(), "core guard leaked");
    let sentinel = line(ROUNDS + 1, 16);
    let mut last = fixture.human();
    let (grant, _) = granted(grant(&mut last, ROUNDS + 1, ROUNDS));
    write(&mut last, grant, ROUNDS + 1, &sentinel);
    assert_eq!(fixture.effects_through(&sentinel), lines(1..=ROUNDS, 16));
}
