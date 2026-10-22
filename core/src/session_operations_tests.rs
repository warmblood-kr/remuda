use super::*;
use crate::agent::{AgentProcess, Cursor, ScreenSnapshot, ScriptedAgent};
use crate::ManualClock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Send(Vec<u8>),
    SendLine(String),
    InputText(String),
    Type(String, Duration),
    Submit(String),
    Clear(Vec<u8>),
    Feed(Vec<Step>),
    Batch(u64, Vec<u8>),
    Resize(Size),
    Terminate,
    Raw(u64, Vec<u8>),
    Snapshot(usize),
    Wait(u64, Duration),
}
struct Shared {
    calls: Mutex<Vec<Call>>,
    unavailable: AtomicBool,
    unknown_input: AtomicBool,
    changed_instance: AtomicBool,
    forgotten: AtomicBool,
    generation: AtomicU64,
    active: AtomicU64,
    human: AtomicBool,
}
impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            unavailable: AtomicBool::new(false),
            unknown_input: AtomicBool::new(false),
            changed_instance: AtomicBool::new(false),
            forgotten: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            active: AtomicU64::new(0),
            human: AtomicBool::new(false),
        })
    }
    fn call(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }
}
struct Remote {
    shared: Arc<Shared>,
}
fn identity() -> Identity {
    Identity {
        host: Some([3; 16]),
        id: "session-id".into(),
        name: "fixture".into(),
        instance: "original-instance".into(),
    }
}
fn request() -> RequestIdentity {
    RequestIdentity {
        session: identity(),
        client: [7; 16],
        sequence: 1,
        payload: [9; 32],
    }
}
impl Remote {
    fn checked(&self) -> Result<()> {
        if self.shared.unavailable.load(Ordering::SeqCst) {
            Err(Error::Unavailable(Unavailable::Transport))
        } else {
            Ok(())
        }
    }
    fn frame(&self) -> VersionedSnapshot {
        VersionedSnapshot {
            instance_id: Some(if self.shared.changed_instance.load(Ordering::SeqCst) {
                "replacement".into()
            } else {
                identity().instance
            }),
            output_version: Some(42),
            snapshot: ScreenSnapshot {
                cells: Vec::new(),
                wrapped: Vec::new(),
                cursor: Cursor {
                    row: 0,
                    col: 0,
                    visible: true,
                },
                scrollback_len: 12,
                scrollback_total: 100,
            },
        }
    }
}
impl SessionOperations for Remote {
    fn identity(&self) -> Identity {
        identity()
    }
    fn input_owner(&self) -> Option<[u8; 16]> {
        Some([7; 16])
    }
    fn inspect(&self) -> Result<Observation> {
        self.checked()?;
        let mut id = identity();
        if self.shared.changed_instance.load(Ordering::SeqCst) {
            id.instance = "replacement".into();
        }
        Ok(Observation {
            identity: id,
            lifecycle: Lifecycle::Running,
            cut: ObservationCut::Host { revision: 42 },
            size: Size::new(90, 25),
            pid: Some(1234),
            output_version: Some(42),
            idle: Duration::from_secs(10),
            output_idle: Duration::from_secs(5),
            human_idle: self
                .shared
                .human
                .load(Ordering::SeqCst)
                .then_some(Duration::ZERO),
            mouse: MouseState::default(),
            attached: self.shared.active.load(Ordering::SeqCst) != 0,
            closing: false,
            host_build: Some("test-host".into()),
        })
    }
    fn recover(&self, key: &RequestIdentity) -> Result<Recovery> {
        self.checked()?;
        if self.shared.forgotten.load(Ordering::SeqCst) {
            return Ok(Recovery::Forgotten);
        }
        if *key != request() {
            return Ok(Recovery::Conflict);
        }
        Ok(Recovery::Complete {
            delivery: Delivery::Written,
            reply: RecoveredReply::Submission(InputSubmitOutcome::Late),
        })
    }
    fn send(&self, bytes: &[u8]) -> Result<()> {
        self.checked()?;
        self.shared.call(Call::Send(bytes.to_vec()));
        Ok(())
    }
    fn send_line(&self, text: &str) -> Result<()> {
        self.checked()?;
        self.shared.call(Call::SendLine(text.into()));
        if self.shared.unknown_input.load(Ordering::SeqCst) {
            Err(Error::Indeterminate(Box::new(request())))
        } else {
            Ok(())
        }
    }
    fn input_text(&self, text: &str) -> Result<()> {
        self.checked()?;
        self.shared.call(Call::InputText(text.into()));
        Ok(())
    }
    fn type_text(&self, text: &str, settle: Duration) -> Result<InputSubmitOutcome> {
        self.checked()?;
        self.shared.call(Call::Type(text.into(), settle));
        Ok(InputSubmitOutcome::Late)
    }
    fn submit(&self, expect: &str) -> Result<InputSubmitOutcome> {
        self.checked()?;
        self.shared.call(Call::Submit(expect.into()));
        Ok(InputSubmitOutcome::Unverified)
    }
    fn clear_input(&self, key: &[u8]) -> Result<Option<String>> {
        self.checked()?;
        self.shared.call(Call::Clear(key.to_vec()));
        Ok(Some("draft".into()))
    }
    fn feed(&self, steps: &[Step]) -> Result<()> {
        self.checked()?;
        self.shared.call(Call::Feed(steps.to_vec()));
        Ok(())
    }
    fn apply_input_batch(&self, batch: InputBatch<'_>) -> Result<InputOutcome> {
        self.checked()?;
        self.shared
            .call(Call::Batch(batch.seq, batch.bytes.to_vec()));
        Ok(InputOutcome::Uncertain)
    }
    fn resize(&self, size: Size) -> Result<()> {
        self.checked()?;
        self.shared.call(Call::Resize(size));
        Ok(())
    }
    fn terminate(&self) -> Result<()> {
        self.checked()?;
        self.shared.call(Call::Terminate);
        Ok(())
    }
    fn screen_text(&self) -> Result<String> {
        self.checked()?;
        Ok("screen".into())
    }
    fn snapshot(&self, scrollback: usize) -> Result<VersionedSnapshot> {
        self.checked()?;
        self.shared.call(Call::Snapshot(scrollback));
        Ok(self.frame())
    }
    fn wait_after(&self, version: u64, timeout: Duration) -> Result<VersionedSnapshot> {
        self.checked()?;
        self.shared.call(Call::Wait(version, timeout));
        Ok(self.frame())
    }
    fn wakeup(&self) -> Result<Option<OutputWakeup>> {
        self.checked()?;
        Ok(None)
    }
    fn attach(&self) -> Result<Box<dyn AttachmentOperations + '_>> {
        self.checked()?;
        let generation = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared.active.store(generation, Ordering::SeqCst);
        Ok(Box::new(Guard {
            shared: self.shared.clone(),
            generation,
        }))
    }
}
struct Guard {
    shared: Arc<Shared>,
    generation: u64,
}
impl AttachmentOperations for Guard {
    fn identity(&self) -> Identity {
        identity()
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn displaced(&self) -> Result<bool> {
        Ok(self.shared.active.load(Ordering::SeqCst) != self.generation)
    }
    fn writer_timed_out(&self) -> Result<bool> {
        Ok(false)
    }
    fn write_raw(
        &self,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
        on_start: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<()> {
        if self.displaced()? || cancelled() {
            return Err(Error::Agent(AgentError::Attached));
        }
        on_start();
        self.shared.call(Call::Raw(self.generation, bytes.to_vec()));
        self.shared.human.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn screen_bytes(&self) -> Result<Vec<u8>> {
        Ok(b"screen".to_vec())
    }
    fn wakeup(&self) -> Result<Option<OutputWakeup>> {
        Ok(None)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.shared.active.compare_exchange(
            self.generation,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}
fn handle(shared: &Arc<Shared>) -> SessionHandle {
    SessionHandle::hosted(Arc::new(Remote {
        shared: shared.clone(),
    }))
    .unwrap()
}

#[test]
fn full_remote_acts_are_never_split_or_retimed_and_rich_replies_are_preserved() {
    let shared = Shared::new();
    let handle = handle(&shared);
    assert!(handle.as_local().is_none());
    handle.send(b"raw").unwrap();
    handle.send_line("line").unwrap();
    handle.input_text("\x1bbody").unwrap();
    assert_eq!(
        handle.type_text("body", Duration::from_secs(3)).unwrap(),
        InputSubmitOutcome::Late
    );
    assert_eq!(
        handle.submit("tail").unwrap(),
        InputSubmitOutcome::Unverified
    );
    assert_eq!(handle.clear_input(b"\x15").unwrap(), Some("draft".into()));
    let feed = [
        Step::Burst(b"first".to_vec()),
        Step::Pause(2000),
        Step::Burst(b"last".to_vec()),
    ];
    handle.feed(&feed).unwrap();
    assert_eq!(
        handle
            .apply_input_batch(InputBatch {
                instance_id: "original-instance",
                client_id: [7; 16],
                seq: 9,
                bytes: b"batch"
            })
            .unwrap(),
        InputOutcome::Uncertain
    );
    handle.resize(Size::new(100, 30)).unwrap();
    handle.terminate().unwrap();
    assert_eq!(
        *shared.calls.lock().unwrap(),
        vec![
            Call::Send(b"raw".to_vec()),
            Call::SendLine("line".into()),
            Call::InputText("\x1bbody".into()),
            Call::Type("body".into(), Duration::from_secs(3)),
            Call::Submit("tail".into()),
            Call::Clear(vec![0x15]),
            Call::Feed(feed.to_vec()),
            Call::Batch(9, b"batch".to_vec()),
            Call::Resize(Size::new(100, 30)),
            Call::Terminate
        ]
    );
}
#[test]
fn one_host_observation_and_versioned_reads_keep_the_supplied_cut() {
    let shared = Shared::new();
    let handle = handle(&shared);
    let observed = handle.inspect().unwrap();
    assert_eq!(observed.cut, ObservationCut::Host { revision: 42 });
    assert_eq!(observed.output_version, Some(42));
    assert_eq!(observed.size, Size::new(90, 25));
    assert_eq!(handle.snapshot(3).unwrap().snapshot.scrollback_total, 100);
    assert_eq!(
        handle
            .wait_after(41, Duration::from_secs(1))
            .unwrap()
            .output_version,
        Some(42)
    );
    assert_eq!(handle.screen_text().unwrap(), "screen");
    assert!(handle.wakeup().unwrap().is_none());
}
#[test]
fn unavailable_owner_refuses_reads_and_attach_without_human_effects() {
    let shared = Shared::new();
    let handle = handle(&shared);
    shared.unavailable.store(true, Ordering::SeqCst);
    assert!(matches!(
        handle.inspect(),
        Err(Error::Unavailable(Unavailable::Transport))
    ));
    assert!(matches!(handle.snapshot(0), Err(Error::Unavailable(_))));
    assert!(matches!(handle.screen_text(), Err(Error::Unavailable(_))));
    assert!(matches!(handle.attach(), Err(Error::Unavailable(_))));
    assert!(!shared.human.load(Ordering::SeqCst));
    assert_eq!(shared.active.load(Ordering::SeqCst), 0);
}
#[test]
fn changed_instance_cannot_supply_a_replacement_observation_or_screen() {
    let shared = Shared::new();
    let handle = handle(&shared);
    shared.changed_instance.store(true, Ordering::SeqCst);
    assert!(matches!(handle.inspect(), Err(Error::Protocol(_))));
    assert!(matches!(handle.snapshot(0), Err(Error::Protocol(_))));
}
#[test]
fn recovery_rejects_wrong_session_client_and_zero_sequence_without_fallback() {
    let shared = Shared::new();
    let handle = handle(&shared);
    let mut changed = request();
    changed.session.host = Some([4; 16]);
    let mut wrong_client = request();
    wrong_client.client = [8; 16];
    let mut zero = request();
    zero.sequence = 0;
    for key in [changed, wrong_client, zero] {
        assert!(matches!(handle.recover(&key), Err(Error::Protocol(_))));
    }
    assert!(shared.calls.lock().unwrap().is_empty());
}
#[test]
fn a_local_owner_cannot_be_relabelled_as_authenticated_remote_or_recover_input() {
    let local = Arc::new(Session::new(
        "local",
        Box::new(ScriptedAgent::new(vec![])),
        Arc::new(ManualClock::default()),
    ));
    assert!(matches!(
        SessionHandle::hosted(local.clone()),
        Err(Error::Protocol(_))
    ));
    let handle = SessionHandle::local(local);
    assert!(matches!(
        handle.recover(&request()),
        Err(Error::Unavailable(Unavailable::Unsupported))
    ));
}
#[test]
fn one_owner_and_uncertain_request_survive_100_adapter_rebuilds_without_replay_or_close() {
    let shared = Shared::new();
    shared.unknown_input.store(true, Ordering::SeqCst);
    let first = handle(&shared);
    let key = match first.send_line("payload") {
        Err(Error::Indeterminate(key)) => key,
        other => panic!("{other:?}"),
    };
    drop(first);
    for _ in 0..100 {
        let replacement = handle(&shared);
        assert_eq!(replacement.input_owner(), Some(key.client));
        assert_eq!(replacement.identity(), &key.session);
        assert!(matches!(
            replacement.recover(&key).unwrap(),
            Recovery::Complete {
                delivery: Delivery::Written,
                reply: RecoveredReply::Submission(InputSubmitOutcome::Late)
            }
        ));
        let mut changed = key.clone();
        changed.payload[0] ^= 1;
        assert!(matches!(
            replacement.recover(&changed).unwrap(),
            Recovery::Conflict
        ));
    }
    shared.forgotten.store(true, Ordering::SeqCst);
    assert!(matches!(
        handle(&shared).recover(&key).unwrap(),
        Recovery::Forgotten
    ));
    assert_eq!(
        *shared.calls.lock().unwrap(),
        vec![Call::SendLine("payload".into())]
    );
}
#[test]
fn stale_attachment_drop_and_writes_cannot_release_or_type_into_successor() {
    let shared = Shared::new();
    let handle = handle(&shared);
    let old = handle.attach().unwrap();
    let current = handle.attach().unwrap();
    assert!(old.displaced().unwrap());
    assert!(!current.writer_timed_out().unwrap());
    assert!(old
        .write_raw(b"wrong", &|| false, Arc::new(|| panic!("stale start")))
        .is_err());
    assert!(current
        .write_raw(
            b"cancelled",
            &|| true,
            Arc::new(|| panic!("cancelled start"))
        )
        .is_err());
    drop(old);
    assert!(!current.displaced().unwrap());
    assert_eq!(current.identity(), handle.identity());
    assert_eq!(current.screen_bytes().unwrap(), b"screen");
    assert!(current.wakeup().unwrap().is_none());
    current
        .write_raw(b"human", &|| false, Arc::new(|| {}))
        .unwrap();
    assert_eq!(handle.inspect().unwrap().human_idle, Some(Duration::ZERO));
    let generation = current.generation();
    drop(current);
    assert_eq!(
        *shared.calls.lock().unwrap(),
        vec![Call::Raw(generation, b"human".to_vec())]
    );
    assert!(!handle.inspect().unwrap().attached);
}

struct ObservedAgent {
    inner: ScriptedAgent,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}
impl AgentProcess for ObservedAgent {
    fn write(&mut self, bytes: &[u8]) -> crate::agent::Result<()> {
        self.writes.lock().unwrap().push(bytes.to_vec());
        self.inner.write(bytes)
    }
    fn screen_text(&mut self) -> crate::agent::Result<String> {
        self.inner.screen_text()
    }
    fn cursor(&mut self) -> crate::agent::Result<Cursor> {
        self.inner.cursor()
    }
    fn is_alive(&mut self) -> bool {
        self.inner.is_alive()
    }
    fn terminate(&mut self) -> crate::agent::Result<()> {
        self.inner.terminate()
    }
    fn size(&self) -> Size {
        self.inner.size()
    }
}
#[test]
fn local_handle_reuses_the_exact_owner_and_its_existing_human_gate() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let session = Arc::new(Session::new(
        "local",
        Box::new(ObservedAgent {
            inner: ScriptedAgent::new(vec![String::new()]),
            writes: writes.clone(),
        }),
        Arc::new(ManualClock::default()),
    ));
    let handle = SessionHandle::local(session.clone());
    assert!(Arc::ptr_eq(handle.as_local().unwrap(), &session));
    handle.send_line("line").unwrap();
    let attachment = handle.attach().unwrap();
    assert!(session.is_attached());
    assert!(matches!(
        handle.terminate(),
        Err(Error::Agent(AgentError::Attached))
    ));
    attachment
        .write_raw(b"human", &|| false, Arc::new(|| {}))
        .unwrap();
    drop(attachment);
    assert!(!session.is_attached());
    assert_eq!(session.human_idle_for(), Some(Duration::ZERO));
    assert!(matches!(
        handle.clear_input(b"\x15"),
        Err(Error::Agent(AgentError::HumanInputRecent))
    ));
    assert_eq!(
        *writes.lock().unwrap(),
        vec![b"line\r".to_vec(), b"human".to_vec()]
    );
    assert_eq!(handle.inspect().unwrap().cut, ObservationCut::LocalFields);
    handle.terminate().unwrap();
    assert_eq!(handle.inspect().unwrap().lifecycle, Lifecycle::Exited(None));
}

fn scripted(name: &str) -> Session {
    Session::new(
        name,
        Box::new(ScriptedAgent::new(vec![])),
        Arc::new(ManualClock::default()),
    )
}
#[test]
fn hosted_and_local_sessions_share_one_registry_name_space() {
    let registry = crate::Registry::new();
    let shared = Shared::new();
    registry.register(scripted("fixture")).unwrap();
    // A hosted handle cannot take a local name, and comes back unregistered.
    assert!(registry.register_hosted(handle(&shared)).is_err());
    assert!(registry.remove("fixture").is_some());
    assert!(registry.register_hosted(handle(&shared)).is_ok());
    assert!(registry.register(scripted("fixture")).is_err());
    assert!(registry.register_hosted(handle(&shared)).is_err());
    assert_eq!(registry.unique_name("fixture"), "fixture-2");
    // Legacy `get` never hands out a hosted session as a local Session.
    assert!(registry.get("fixture").is_none());
    assert!(registry.hosted("fixture").is_some());
}
#[test]
fn name_routes_reach_a_hosted_handle_and_keep_its_errors() {
    let registry = crate::Registry::new();
    let shared = Shared::new();
    assert!(registry.register_hosted(handle(&shared)).is_ok());
    registry.send_line("fixture", "line").unwrap().unwrap();
    registry.send("fixture", b"raw").unwrap().unwrap();
    registry
        .feed("fixture", &[Step::Pause(1)])
        .unwrap()
        .unwrap();
    registry
        .resize("fixture", Size::new(100, 30))
        .unwrap()
        .unwrap();
    assert_eq!(registry.screen_text("fixture").unwrap().unwrap(), "screen");
    assert_eq!(
        registry.clear_input("fixture", b"\x15").unwrap().unwrap(),
        Some("draft".into())
    );
    assert_eq!(
        *shared.calls.lock().unwrap(),
        vec![
            Call::SendLine("line".into()),
            Call::Send(b"raw".to_vec()),
            Call::Feed(vec![Step::Pause(1)]),
            Call::Resize(Size::new(100, 30)),
            Call::Clear(b"\x15".to_vec()),
        ]
    );
    let listed = registry.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].instance_id.as_deref(), Some("original-instance"));
    assert_eq!(listed[0].size, Size::new(90, 25));
    assert!(listed[0].alive);
    // Unknown outcomes keep their no-retry meaning on the legacy route.
    shared.unknown_input.store(true, Ordering::SeqCst);
    let error = registry.send_line("fixture", "again").unwrap().unwrap_err();
    assert!(error.to_string().contains("recover the original request"));
    shared.unavailable.store(true, Ordering::SeqCst);
    let error = registry.screen_text("fixture").unwrap().unwrap_err();
    assert!(error.to_string().contains("unavailable"));
    // An unreachable host is still listed: not exited, only unavailable.
    assert!(registry.list()[0].alive);
}
#[test]
fn closing_a_hosted_session_checks_its_instance_then_terminates_once() {
    let registry = crate::Registry::new();
    let shared = Shared::new();
    assert!(registry.register_hosted(handle(&shared)).is_ok());
    assert!(registry
        .close_instance("fixture", "replacement")
        .unwrap()
        .is_err());
    assert!(shared.calls.lock().unwrap().is_empty());
    assert!(registry
        .close_instance("fixture", "original-instance")
        .unwrap()
        .unwrap());
    assert_eq!(*shared.calls.lock().unwrap(), vec![Call::Terminate]);
    assert!(registry.hosted("fixture").is_none());
    assert!(registry.close("fixture").is_none());
}
#[test]
fn a_hosted_session_id_names_its_session_for_the_shutdown_guard() {
    let registry = crate::Registry::new();
    let shared = Shared::new();
    assert!(registry.name_for_id("session-id").is_none());
    assert!(registry.register_hosted(handle(&shared)).is_ok());
    assert_eq!(
        registry.name_for_id("session-id").as_deref(),
        Some("fixture")
    );
    assert!(registry.name_for_id("other-id").is_none());
    let handles = registry.hosted_handles();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].identity().name, "fixture");
}
