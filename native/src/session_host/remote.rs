//! Daemon-side SessionOperations adapter over one stable host (issue #713).
//! It translates whole operations and owns no PTY, input clock or dedup state.
//! Every input act is reserved durably before any effect and resolved through
//! the host ledger by its original key, so a lost reply is recovered, never
//! replayed. Results are retained up to a bound, then retired as Forgotten.
//! Expected identity and endpoint come only from the private inventory.
//! The opt-in daemon path (`hosted`) registers it; attach and legacy raw
//! batches are refused as Unsupported for now.
use super::api::{Identity as HostIdentity, Lifecycle as HostLifecycle, Request, Response, Role};
use super::entry::Client;
use super::inventory::disk::Error as StoreError;
use super::inventory::model::{self, Entry, Index, Key, Lookup, Phase, Target};
use super::recovery::{Owner, Progress};
use super::result::{Completion, Delivery as HostDelivery, ErrorCode, Reply, Submission};
use super::viewport::Viewport;
use super::wire::{InputAct, Step as WireStep};
use remuda_core::agent::VersionedSnapshot;
use remuda_core::agent::{AgentError, ExitInfo, OutputWakeup, ScreenSnapshot, Size};
use remuda_core::input::{InputBatch, InputOutcome};
use remuda_core::protocol::Step;
use remuda_core::session::InputSubmitOutcome;
use remuda_core::session_operations::{
    AttachmentOperations, Delivery, Error, Identity, Lifecycle, Observation, ObservationCut,
    RecoveredReply, Recovery, RequestIdentity, Result, SessionOperations, Unavailable,
};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(10);
const MAX_POLL: Duration = Duration::from_millis(100);
const ACT_DEADLINE: Duration = Duration::from_secs(30);
const CONTROL_DEADLINE: Duration = Duration::from_secs(5);
const MAX_WATCH: Duration = Duration::from_millis(750);
const WATCH_GAP: Duration = Duration::from_millis(25);
/// Below the store's per-client result cap, so a new reservation always fits.
const RETAINED: usize = model::MAX_RESULTS / 2;

/// What a host made of [`HostSession::announce_writer`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Announced {
    /// It now refuses every older writer.
    Fenced,
    /// It predates `writer-fence-v1`: only the inventory fence guards it.
    Unfenced,
    /// It has seen a newer writer than ours.
    Superseded,
    Unreachable(String),
}

pub(crate) struct HostSession {
    owner: Arc<Owner>,
    name: String,
    identity: HostIdentity,
    endpoint: PathBuf,
    client: [u8; 16],
    acts: Mutex<()>,
    gate: Arc<super::gate::Gate>,
}
impl HostSession {
    /// `client` is the daemon's stable input identity for this host incarnation.
    pub fn open(owner: Arc<Owner>, name: &str, client: [u8; 16]) -> Result<Self> {
        let index = owner.index().map_err(store)?;
        let entry = index
            .entries
            .get(name)
            .ok_or(Error::Unavailable(Unavailable::Recovering))?;
        if entry.phase == Phase::Reserved {
            return Err(Error::Unavailable(Unavailable::Recovering));
        }
        let birth = entry
            .birth
            .as_ref()
            .ok_or(Error::Unavailable(Unavailable::Unsupported))?;
        let session = Self {
            owner,
            name: name.into(),
            identity: entry.identity.clone(),
            endpoint: PathBuf::from(birth.endpoint()),
            client,
            acts: Mutex::new(()),
            gate: Arc::default(),
        };
        session.identity().validate()?;
        Ok(session)
    }
    /// Admit this session's acts and control calls through `gate`, so a
    /// daemon drain can stop and wait for them.
    pub fn gated(mut self, gate: Arc<super::gate::Gate>) -> Self {
        self.gate = gate;
        self
    }
    fn target(&self) -> Target {
        Target {
            name: self.name.clone(),
            identity: self.identity.clone(),
        }
    }
    fn entry<'a>(&self, index: &'a Index) -> Result<&'a Entry> {
        let entry = index
            .entries
            .get(&self.name)
            .ok_or(Error::Unavailable(Unavailable::Recovering))?;
        if entry.identity != self.identity {
            return Err(Error::Protocol("inventory session identity changed"));
        }
        Ok(entry)
    }
    fn request(&self, key: &Key) -> Box<RequestIdentity> {
        Box::new(RequestIdentity {
            session: self.identity(),
            client: key.client,
            sequence: key.sequence,
            payload: key.payload,
        })
    }
    /// Run one whole act: reserve, drive through the host, then publish receipt.
    fn act(&self, act: InputAct) -> Result<Completion> {
        act.validate().map_err(|reason| match reason {
            // Kept small so an older build can still read the inventory that
            // records every act; callers must split larger input (#713).
            "input act limit exceeded" => Error::Agent(AgentError::Io(format!(
                "hosted input acts are limited to {} KiB; split the message",
                super::wire::MAX_INPUT / 1024
            ))),
            reason => Error::Protocol(reason),
        })?;
        let _pass = self.gate.enter()?;
        let deadline = ACT_DEADLINE + Duration::from_millis(pauses(&act));
        let _serial = self.acts.lock().unwrap_or_else(|e| e.into_inner());
        // An unreachable host is a clean refusal while nothing is reserved yet.
        drop(Client::open_observer(&self.endpoint, self.identity.clone()).map_err(opened)?);
        let key = self.reserve(act)?;
        let until = Instant::now() + deadline;
        let mut delay = POLL;
        // A host that stays unreachable leaves the act recoverable by its key,
        // instead of holding the caller for the whole act deadline.
        let mut unreachable: Option<Instant> = None;
        loop {
            // After the reservation a failure leaves the outcome unknown; only
            // store contention (lock or revision race) is retried in place.
            let progress = match self
                .owner
                .resume(self.target(), key.clone(), &self.endpoint)
            {
                Err(error) if contended(&error) && Instant::now() < until => Progress::Busy,
                Err(_) => return Err(Error::Indeterminate(self.request(&key))),
                Ok(progress) => progress,
            };
            match progress {
                // A failure that may have written stays recoverable evidence.
                Progress::Complete(completion)
                    if matches!(completion.reply, Reply::Error(_))
                        && completion.delivery != HostDelivery::Rejected =>
                {
                    return Err(Error::Indeterminate(self.request(&key)));
                }
                Progress::Complete(completion) => {
                    self.settle(&key);
                    return Ok(completion);
                }
                Progress::Unavailable
                    if unreachable.is_some_and(|since| since.elapsed() >= CONTROL_DEADLINE) =>
                {
                    return Err(Error::Indeterminate(self.request(&key)));
                }
                Progress::Pending | Progress::Busy | Progress::Unavailable
                    if Instant::now() < until =>
                {
                    if matches!(progress, Progress::Unavailable) {
                        unreachable.get_or_insert_with(Instant::now);
                    } else {
                        unreachable = None;
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(MAX_POLL);
                }
                Progress::Forgotten => return Err(Error::Forgotten(self.request(&key))),
                _ => return Err(Error::Indeterminate(self.request(&key))),
            }
        }
    }
    /// Store contention before publication is a clean, retryable refusal.
    fn reserve(&self, act: InputAct) -> Result<Key> {
        let until = Instant::now() + CONTROL_DEADLINE;
        loop {
            match self.try_reserve(act.clone()) {
                Err(Attempt::Contended) if Instant::now() < until => std::thread::sleep(POLL),
                Err(Attempt::Contended) => return Err(Error::Unavailable(Unavailable::Recovering)),
                Err(Attempt::Failed(error)) => return Err(error),
                Ok(key) => return Ok(key),
            }
        }
    }
    fn try_reserve(&self, act: InputAct) -> std::result::Result<Key, Attempt> {
        let index = self.owner.index().map_err(store)?;
        let entry = self.entry(&index)?;
        // One act per session. An earlier act (for example one an old daemon
        // reserved) is first resolved by its original key, never re-issued as
        // new input. Its result stays retained, so `recover` still answers it.
        if let Some(pending) = &entry.pending {
            let key = pending.key.clone();
            return match self
                .owner
                .resume(self.target(), key.clone(), &self.endpoint)
            {
                Ok(Progress::Complete(_) | Progress::Forgotten) => Err(Attempt::Contended),
                Err(error) if contended(&error) => Err(Attempt::Contended),
                _ => Err(Error::Indeterminate(self.request(&key)).into()),
            };
        }
        // Published exit: a clean refusal before any reservation or effect.
        if matches!(entry.phase, Phase::Exited { .. }) {
            return Err(Error::Agent(AgentError::Exited).into());
        }
        self.evict(entry)?;
        let sequence = match entry.streams.iter().find(|s| s.client == self.client) {
            Some(stream) => stream
                .next
                .ok_or(Error::Protocol("input sequence exhausted"))?,
            None => 1,
        };
        let mut intent = [0; 16];
        getrandom::fill(&mut intent).map_err(|_| Error::Unavailable(Unavailable::Transport))?;
        match self
            .owner
            .reserve(self.target(), intent, self.client, sequence, act)
        {
            Ok(key) => Ok(key),
            Err(error) => {
                // An unknown commit may have published; only our intent proceeds.
                let index = self.owner.index().map_err(store)?;
                match &self.entry(&index)?.pending {
                    Some(pending) if pending.intent == intent => Ok(pending.key.clone()),
                    Some(pending) => Err(Error::Indeterminate(self.request(&pending.key)).into()),
                    None if contended(&error) => Err(Attempt::Contended),
                    None => Err(store(error).into()),
                }
            }
        }
    }
    /// Bounded retention: past RETAINED unreceipted results, the oldest are
    /// retired and then answer Forgotten; this never reopens or replays them.
    fn evict(&self, entry: &Entry) -> std::result::Result<(), Attempt> {
        let Some(stream) = entry.streams.iter().find(|s| s.client == self.client) else {
            return Ok(());
        };
        let open: Vec<_> = stream
            .results
            .iter()
            .filter(|result| !result.consumer_ack)
            .collect();
        for result in open.iter().take((open.len() + 1).saturating_sub(RETAINED)) {
            self.owner
                .consume(
                    self.target(),
                    result.reservation.key.clone(),
                    result.reservation.intent,
                    &result.completion,
                )
                .map_err(|error| {
                    if contended(&error) {
                        Attempt::Contended
                    } else {
                        Attempt::Failed(store(error))
                    }
                })?;
        }
        Ok(())
    }
    /// Best effort: an unpublished receipt only keeps the result recoverable.
    fn settle(&self, key: &Key) {
        let Ok(index) = self.owner.index() else {
            return;
        };
        if let Ok(Lookup::Complete(result)) = index.lookup(&self.target(), key) {
            if !result.consumer_ack {
                let _ = self.owner.consume(
                    self.target(),
                    key.clone(),
                    result.reservation.intent,
                    &result.completion,
                );
            }
        }
    }
    /// After `terminate`: wait for the published exit, resolve a pending act
    /// by its key, retire retained results, stop the host, then retire the
    /// inventory entry. Every step is idempotent, so a later retry may resume.
    pub fn teardown(&self, deadline: Duration) -> Result<()> {
        let until = Instant::now() + deadline;
        let mut stopped = false;
        loop {
            match self.teardown_step(&mut stopped) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) if Instant::now() >= until => return Err(error),
                Err(_) => {}
            }
            if Instant::now() >= until {
                return Err(Error::Unavailable(Unavailable::Recovering));
            }
            std::thread::sleep(MAX_POLL);
        }
    }
    fn teardown_step(&self, stopped: &mut bool) -> Result<bool> {
        // An act in flight here (say, input that ended the child) collects and
        // settles its own result first; teardown only resolves orphaned keys.
        let _serial = self.acts.lock().unwrap_or_else(|e| e.into_inner());
        let index = self.owner.index().map_err(store)?;
        let Some(entry) = index.entries.get(&self.name) else {
            return Ok(true);
        };
        if entry.identity != self.identity {
            return Ok(true);
        }
        if !matches!(entry.phase, Phase::Exited { .. }) {
            // A gone host never publishes an exit; nothing more can settle.
            return Ok(self.host_gone());
        }
        if let Some(pending) = &entry.pending {
            self.owner
                .resume(self.target(), pending.key.clone(), &self.endpoint)
                .map_err(store)?;
            return Ok(false);
        }
        for stream in &entry.streams {
            for result in stream.results.iter().filter(|r| !r.consumer_ack) {
                self.owner
                    .consume(
                        self.target(),
                        result.reservation.key.clone(),
                        result.reservation.intent,
                        &result.completion,
                    )
                    .map_err(store)?;
            }
        }
        // A host already stopped (say, before a crash) has no listener left;
        // the store still refuses Retire unless the exit is settled.
        if !*stopped && !self.host_gone() {
            let mut client = self.controller()?;
            match client.request(&Request::Stop).map_err(transport)? {
                Response::Ok => *stopped = true,
                Response::Error { code } => return Err(refused(&code)),
                _ => return Err(Error::Protocol("unexpected host stop reply")),
            }
        }
        self.owner.retire(self.target()).map_err(store)?;
        Ok(true)
    }
    /// No listener at the endpoint: the host process is gone. Any other
    /// failure (a slow or busy host) is not proof.
    fn wait_for_frame(&self, version: u64, timeout: Duration) -> Result<VersionedSnapshot> {
        let until = Instant::now() + timeout;
        let mut client = self.viewer()?;
        loop {
            let view = self.capture(&mut client, 0)?;
            if view.generation > version {
                return Ok(self.versioned(view));
            }
            if !self.watch(&mut client, view.generation, until)? {
                // As an in-process wait does: nothing new and the child gone
                // is an exit, not a frame.
                if matches!(
                    SessionOperations::inspect(self)?.lifecycle,
                    Lifecycle::Exited(_)
                ) {
                    return Err(Error::Agent(AgentError::Exited));
                }
                return Ok(self.versioned(view));
            }
        }
    }
    fn host_gone(&self) -> bool {
        match std::os::unix::net::UnixStream::connect(self.endpoint.join("socket")) {
            Ok(_) => false,
            Err(error) => matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ),
        }
    }
    /// Non-input control (resize, close, stop) is not reserved in the store.
    /// A fencing host refuses a superseded owner atomically; the store check
    /// below is the only guard on an older host without writer-fence-v1.
    fn controller(&self) -> Result<Client> {
        if !self.owner.current().map_err(store)? {
            return Err(Error::Unavailable(Unavailable::Recovering));
        }
        let until = Instant::now() + CONTROL_DEADLINE;
        loop {
            let mut client =
                Client::open_expected(&self.endpoint, Role::Control, self.identity.clone())
                    .map_err(opened)?;
            match client.acquire(self.owner.writer()).map_err(transport)? {
                Response::Granted { .. } => return Ok(client),
                Response::Error { code } if code == "controller_busy" && Instant::now() < until => {
                    std::thread::sleep(POLL);
                }
                Response::Error { code } => return Err(refused(&code)),
                _ => return Err(Error::Protocol("unexpected host acquire reply")),
            }
        }
    }
    /// Show the host this daemon's current writer, so it refuses older ones
    /// from now on, without waiting for this daemon's next act. Takes the
    /// lease only for that moment.
    pub fn announce_writer(&self) -> Announced {
        let mut client =
            match Client::open_expected(&self.endpoint, Role::Control, self.identity.clone()) {
                Ok(client) => client,
                Err(error) => return Announced::Unreachable(error.to_string()),
            };
        let fences = client.offers(super::api::WRITER_FENCE);
        let fenced = if fences {
            Announced::Fenced
        } else {
            Announced::Unfenced
        };
        match client.acquire(self.owner.writer()) {
            Ok(Response::Granted { .. }) => {
                let _ = client.request(&Request::Release);
                fenced
            }
            // The host raised its writer high-water before the lease check.
            Ok(Response::Error { code }) if code == "controller_busy" => fenced,
            Ok(Response::Error { code }) if code == "superseded_writer" => Announced::Superseded,
            Ok(Response::Error { code }) => Announced::Unreachable(code),
            Ok(_) => Announced::Unreachable("unexpected host acquire reply".into()),
            Err(error) => Announced::Unreachable(error.to_string()),
        }
    }
    fn control(&self, request: Request) -> Result<()> {
        let _pass = self.gate.enter()?;
        let mut client = self.controller()?;
        match client.request(&request).map_err(transport)? {
            Response::Ok => Ok(()),
            Response::Error { code } => Err(refused(&code)),
            _ => Err(Error::Protocol("unexpected host control reply")),
        }
    }
    fn viewer(&self) -> Result<Client> {
        Client::open_viewport(&self.endpoint, self.identity.clone()).map_err(opened)
    }
    /// One acknowledged typed viewport; the ACK arms this connection's watch.
    fn capture(&self, client: &mut Client, offset: u16) -> Result<Viewport> {
        let until = Instant::now() + CONTROL_DEADLINE;
        let (snapshot, generation, bytes, pages) = loop {
            // An older host knows only `Viewport`, within the legacy history.
            let request = if client.offers(super::api::COMPACT_VIEW) {
                Request::ViewportCompact { scrollback: offset }
            } else if client.offers(super::api::DEEP_HISTORY) {
                Request::ViewportDeep { scrollback: offset }
            } else {
                Request::Viewport { scrollback: offset }
            };
            match client.request(&request).map_err(transport)? {
                Response::Viewport {
                    identity,
                    snapshot,
                    generation,
                    bytes,
                    pages,
                    ..
                } if identity == self.identity => break (snapshot, generation, bytes, pages),
                Response::Error { code }
                    if code == "snapshot_budget_busy" && Instant::now() < until =>
                {
                    std::thread::sleep(POLL);
                }
                Response::Error { code } => return Err(refused(&code)),
                _ => return Err(Error::Protocol("unexpected host viewport reply")),
            }
        };
        if bytes > super::api::MAX_SNAPSHOT_BYTES || pages != bytes.div_ceil(super::api::PAGE_BYTES)
        {
            return Err(Error::Protocol("host viewport exceeds its budget"));
        }
        let mut assembled = Vec::with_capacity(bytes);
        for index in 0..pages {
            let Response::Page {
                snapshot: reply,
                index: page,
                base64,
            } = client
                .request(&Request::Page { snapshot, index })
                .map_err(transport)?
            else {
                return Err(Error::Protocol("host viewport page refused"));
            };
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD
                .decode(base64)
                .map_err(|_| Error::Protocol("host viewport page encoding"))?;
            if reply != snapshot
                || page != index
                || data.len() != (bytes - assembled.len()).min(super::api::PAGE_BYTES)
            {
                return Err(Error::Protocol("host viewport page identity"));
            }
            assembled.extend_from_slice(&data);
        }
        let view =
            Viewport::decode(&assembled).map_err(|_| Error::Protocol("host viewport decoding"))?;
        view.validate()
            .map_err(|_| Error::Protocol("host viewport bounds"))?;
        if view.generation != generation {
            return Err(Error::Protocol("host viewport generation"));
        }
        match client
            .request(&Request::AcknowledgeViewport {
                snapshot,
                generation,
            })
            .map_err(transport)?
        {
            Response::ViewState { .. } | Response::Ok => Ok(view),
            Response::Error { code } => Err(refused(&code)),
            _ => Err(Error::Protocol("unexpected host viewport ACK reply")),
        }
    }
    fn versioned(&self, view: Viewport) -> VersionedSnapshot {
        VersionedSnapshot {
            output_version: Some(view.generation),
            instance_id: Some(self.identity.instance.clone()),
            snapshot: view.into_screen(),
        }
    }
    /// Bounded watch on an acknowledged viewport; true when output may differ.
    fn watch(&self, client: &mut Client, generation: u64, until: Instant) -> Result<bool> {
        loop {
            let wait = until
                .saturating_duration_since(Instant::now())
                .min(MAX_WATCH);
            if wait.is_zero() {
                return Ok(false);
            }
            match client
                .request(&Request::WatchAfter {
                    generation,
                    timeout_ms: u16::try_from(wait.as_millis()).unwrap_or(u16::MAX),
                })
                .map_err(transport)?
            {
                Response::ViewState {
                    generation: now,
                    state,
                    ..
                } => {
                    if now != generation {
                        return Ok(true);
                    }
                    if state != super::api::ViewState::Unchanged {
                        // Ended/Degraded at the same generation: nothing new arrives.
                        return Ok(false);
                    }
                }
                Response::Error { code } if code == "watch_request_cooldown" => {
                    std::thread::sleep(WATCH_GAP);
                }
                Response::Error { code } => return Err(refused(&code)),
                _ => return Err(Error::Protocol("unexpected host watch reply")),
            }
        }
    }
}
impl SessionOperations for HostSession {
    fn identity(&self) -> Identity {
        Identity {
            host: Some(self.identity.host),
            id: self.identity.session.clone(),
            name: self.name.clone(),
            instance: self.identity.instance.clone(),
        }
    }
    fn input_owner(&self) -> Option<[u8; 16]> {
        Some(self.client)
    }
    fn inspect(&self) -> Result<Observation> {
        let mut client =
            Client::open_observer(&self.endpoint, self.identity.clone()).map_err(opened)?;
        let info = match client.request(&Request::Inspect).map_err(transport)? {
            Response::Info { info } => info,
            Response::Error { code } => return Err(refused(&code)),
            _ => return Err(Error::Protocol("unexpected host inspect reply")),
        };
        if info.identity != self.identity {
            return Err(Error::Protocol("host identity changed"));
        }
        let activity = info
            .activity
            .ok_or(Error::Protocol("host omitted negotiated activity"))?;
        let lifecycle = match info.lifecycle {
            HostLifecycle::Running => Lifecycle::Running,
            HostLifecycle::Exited => Lifecycle::Exited(Some(ExitInfo {
                exit_code: info.exit_code,
                signal: info.signal,
                // Named from the number, as in-process: an older host sent
                // the platform's description ("Terminated: 15").
                signal_name: info.signal.and_then(crate::pty::signal_name),
            })),
            // An unavailable host is never reported as an exited session.
            HostLifecycle::Unavailable => return Err(Error::Unavailable(Unavailable::Recovering)),
        };
        Ok(Observation {
            identity: self.identity(),
            lifecycle,
            cut: ObservationCut::Host {
                revision: info.generation,
            },
            // The host's real size: the daemon already floored it, or the
            // caller opted out of the floor.
            size: Size::for_viewport(info.cols, info.rows),
            pid: info.pid,
            output_version: Some(info.generation),
            idle: Duration::from_millis(activity.idle_ms),
            output_idle: Duration::from_millis(activity.output_idle_ms),
            human_idle: activity.human_idle_ms.map(Duration::from_millis),
            mouse: activity.mouse,
            attached: activity.attached,
            closing: activity.closing,
            host_build: Some(info.host_build),
        })
    }
    fn recover(&self, request: &RequestIdentity) -> Result<Recovery> {
        if request.session != self.identity() {
            return Ok(Recovery::Conflict);
        }
        let key = Key {
            client: request.client,
            sequence: request.sequence,
            payload: request.payload,
        };
        let progress = self
            .owner
            .resume(self.target(), key.clone(), &self.endpoint)
            .map_err(store)?;
        Ok(match progress {
            Progress::Pending | Progress::Busy => Recovery::Pending,
            Progress::Complete(completion) => {
                self.settle(&key);
                Recovery::Complete {
                    delivery: delivery(completion.delivery),
                    reply: match completion.reply {
                        Reply::Unit => RecoveredReply::Unit,
                        Reply::Submission(value) => RecoveredReply::Submission(submission(value)),
                        Reply::Cleared(value) => RecoveredReply::Cleared(value),
                        Reply::Error(code) => RecoveredReply::Failed(code_error(code)),
                    },
                }
            }
            Progress::Forgotten => Recovery::Forgotten,
            Progress::Conflict => Recovery::Conflict,
            Progress::OutOfOrder => Recovery::OutOfOrder,
            Progress::Indeterminate => return Err(Error::Indeterminate(self.request(&key))),
            Progress::Unavailable => return Err(Error::Unavailable(Unavailable::Transport)),
        })
    }
    fn send(&self, bytes: &[u8]) -> Result<()> {
        unit(self.act(InputAct::Bytes(bytes.to_vec()))?)
    }
    fn send_line(&self, text: &str) -> Result<()> {
        unit(self.act(InputAct::SendLine(text.into()))?)
    }
    fn input_text(&self, text: &str) -> Result<()> {
        unit(self.act(InputAct::InputText(text.into()))?)
    }
    fn type_text(&self, text: &str, settle: Duration) -> Result<InputSubmitOutcome> {
        let settle_ms = u64::try_from(settle.as_millis())
            .map_err(|_| Error::Protocol("settle duration overflow"))?;
        submitted(self.act(InputAct::TypeText {
            text: text.into(),
            settle_ms,
        })?)
    }
    fn submit(&self, expect: &str) -> Result<InputSubmitOutcome> {
        submitted(self.act(InputAct::Submit(expect.into()))?)
    }
    fn clear_input(&self, key: &[u8]) -> Result<Option<String>> {
        match reply(self.act(InputAct::Clear(key.to_vec()))?)? {
            Reply::Cleared(value) => Ok(value),
            _ => Err(Error::Protocol("unexpected host clear reply")),
        }
    }
    fn feed(&self, steps: &[Step]) -> Result<()> {
        let steps = steps
            .iter()
            .map(|step| match step {
                Step::Burst(bytes) => WireStep::Bytes(bytes.clone()),
                Step::Pause(ms) => WireStep::Pause(*ms),
            })
            .collect();
        unit(self.act(InputAct::Feed(steps))?)
    }
    fn apply_input_batch(&self, _: InputBatch<'_>) -> Result<InputOutcome> {
        Err(Error::Unavailable(Unavailable::Unsupported))
    }
    fn resize(&self, size: Size) -> Result<()> {
        self.control(Request::Resize {
            rows: size.rows(),
            cols: size.cols(),
        })
    }
    /// After Close, or when the host is provably gone (its PTY closed with
    /// it), the close intent is recorded so a restart does not re-register.
    fn terminate(&self) -> Result<()> {
        match self.control(Request::Close) {
            Ok(()) => {}
            Err(Error::Unavailable(Unavailable::Transport)) if self.host_gone() => {}
            Err(error) => return Err(error),
        }
        // Best effort: an exited child is torn down by discovery anyway.
        let _ = self.owner.mark_closing(self.target());
        Ok(())
    }
    fn screen_text(&self) -> Result<String> {
        Ok(text(&self.snapshot(0)?.snapshot))
    }
    fn snapshot(&self, scrollback: usize) -> Result<VersionedSnapshot> {
        let mut client = self.viewer()?;
        let reach = if client.offers(super::api::DEEP_HISTORY) {
            super::terminal::HISTORY_ROWS
        } else {
            usize::from(super::terminal::LEGACY_HISTORY_ROWS)
        };
        let offset = u16::try_from(scrollback.min(reach)).unwrap_or(u16::MAX);
        Ok(self.versioned(self.capture(&mut client, offset)?))
    }
    fn wait_after(&self, version: u64, timeout: Duration) -> Result<VersionedSnapshot> {
        match self.wait_for_frame(version, timeout) {
            // The child exited and the host stopped with it mid-wait: that is
            // the exit an in-process wait reports, not a transport fault.
            Err(Error::Unavailable(Unavailable::Transport)) if self.host_gone() => {
                Err(Error::Agent(AgentError::Exited))
            }
            result => result,
        }
    }
    /// Output wakes from one persistent observer connection that polls the
    /// host's output generation (Inspect carries it; no screen is copied).
    /// The wake disconnects once the child stops running or the host goes.
    fn wakeup(&self) -> Result<Option<OutputWakeup>> {
        let client =
            Client::open_observer(&self.endpoint, self.identity.clone()).map_err(opened)?;
        let version = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (signal, wakeup) = OutputWakeup::pair(Arc::clone(&version));
        std::thread::Builder::new()
            .name("session-host-output".into())
            .spawn(move || watch_generation(client, &signal, &version))
            .map_err(|_| Error::Unavailable(Unavailable::Transport))?;
        Ok(Some(wakeup))
    }
    fn attach(&self) -> Result<Box<dyn AttachmentOperations + '_>> {
        Err(Error::Unavailable(Unavailable::Unsupported))
    }
}
/// Fastest and slowest generation polls: fast right after output, backing
/// off while the session is quiet.
const GENERATION_POLL: Duration = Duration::from_millis(50);
const GENERATION_IDLE_POLL: Duration = Duration::from_millis(250);

fn watch_generation(
    mut client: Client,
    signal: &remuda_core::agent::OutputSignal,
    version: &std::sync::atomic::AtomicU64,
) {
    use std::sync::atomic::Ordering;
    let mut poll = GENERATION_POLL;
    while signal.is_connected() {
        let Ok(Response::Info { info }) = client.request(&Request::Inspect) else {
            return;
        };
        if info.generation > version.load(Ordering::Acquire) {
            version.store(info.generation, Ordering::Release);
            if !signal.wake() {
                return;
            }
            poll = GENERATION_POLL;
        } else {
            poll = (poll * 2).min(GENERATION_IDLE_POLL);
        }
        if !matches!(info.lifecycle, HostLifecycle::Running) {
            return;
        }
        std::thread::sleep(poll);
    }
}
fn pauses(act: &InputAct) -> u64 {
    match act {
        InputAct::TypeText { settle_ms, .. } => *settle_ms,
        InputAct::Feed(steps) => steps
            .iter()
            .map(|step| match step {
                WireStep::Pause(ms) => *ms,
                WireStep::Bytes(_) => 0,
            })
            .fold(0, u64::saturating_add),
        _ => 0,
    }
}
fn reply(completion: Completion) -> Result<Reply> {
    match completion.reply {
        Reply::Error(code) => Err(code_error(code)),
        reply => Ok(reply),
    }
}
fn unit(completion: Completion) -> Result<()> {
    match reply(completion)? {
        Reply::Unit => Ok(()),
        _ => Err(Error::Protocol("unexpected host input reply")),
    }
}
fn submitted(completion: Completion) -> Result<InputSubmitOutcome> {
    match reply(completion)? {
        Reply::Submission(value) => Ok(submission(value)),
        _ => Err(Error::Protocol("unexpected host submit reply")),
    }
}
fn submission(value: Submission) -> InputSubmitOutcome {
    match value {
        Submission::Submitted => InputSubmitOutcome::Submitted,
        Submission::Unverified => InputSubmitOutcome::Unverified,
        Submission::Late => InputSubmitOutcome::Late,
    }
}
fn delivery(value: HostDelivery) -> Delivery {
    match value {
        HostDelivery::Rejected => Delivery::Rejected,
        HostDelivery::Written => Delivery::Written,
        HostDelivery::Indeterminate => Delivery::Indeterminate,
    }
}
/// Durations inside timeout errors are not carried on the host wire.
fn code_error(code: ErrorCode) -> Error {
    Error::Agent(match code {
        ErrorCode::Busy => AgentError::Busy,
        ErrorCode::Exited => AgentError::Exited,
        ErrorCode::Attached => AgentError::Attached,
        ErrorCode::HumanInputRecent => AgentError::HumanInputRecent,
        ErrorCode::WriteTimeout => AgentError::WriteTimeout {
            timeout: Duration::ZERO,
        },
        ErrorCode::SubmitUncertain => AgentError::SubmitUncertain,
        ErrorCode::LateSubmitAbandoned => AgentError::LateSubmitAbandoned {
            bound: Duration::ZERO,
        },
        ErrorCode::Unavailable => AgentError::Io("host input unavailable".into()),
        ErrorCode::Invalid => return Error::Protocol("host refused invalid input"),
        ErrorCode::ResultTooLarge => return Error::Protocol("host result exceeded its budget"),
    })
}
fn refused(code: &str) -> Error {
    match code {
        "controller_busy" | "snapshot_budget_busy" => Error::Agent(AgentError::Busy),
        // A stopping or unpublished host is not an exited session.
        "host_stopping" | "host_publication_pending" | "host_unavailable" | "superseded_writer" => {
            Error::Unavailable(Unavailable::Recovering)
        }
        _ => Error::Protocol("host refused the operation"),
    }
}
enum Attempt {
    Contended,
    Failed(Error),
}
impl From<Error> for Attempt {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}
fn contended(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Busy | StoreError::Data(model::Error::StaleRevision)
    )
}
fn store(error: StoreError) -> Error {
    match error {
        StoreError::Data(model::Error::Incompatible) => {
            Error::Unavailable(Unavailable::Incompatible)
        }
        _ => Error::Unavailable(Unavailable::Recovering),
    }
}
fn opened(error: io::Error) -> Error {
    Error::Unavailable(match error.kind() {
        io::ErrorKind::PermissionDenied => Unavailable::Authentication,
        io::ErrorKind::InvalidData => Unavailable::Incompatible,
        _ => Unavailable::Transport,
    })
}
fn transport(_: io::Error) -> Error {
    Error::Unavailable(Unavailable::Transport)
}
/// Plain rows like vt100 `contents()`: wide continuations add nothing.
fn text(screen: &ScreenSnapshot) -> String {
    let mut out = String::new();
    for (row, wrapped) in screen.cells.iter().zip(&screen.wrapped) {
        let mut line = String::new();
        let mut continuation = false;
        for cell in row {
            if continuation {
                continuation = false;
                continue;
            }
            line.push_str(if cell.text.is_empty() {
                " "
            } else {
                &cell.text
            });
            continuation = cell.wide;
        }
        out.push_str(line.trim_end());
        if !wrapped {
            out.push('\n');
        }
    }
    out.trim_end_matches('\n').to_owned()
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
