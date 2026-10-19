//! Experimental single-session server. No daemon integration or discovery index.
use super::api::{Lifecycle, Open, Request, Response, Role};
use super::channel::Channel;
use super::control::{ConnectionId, Fence};
use super::coordinator::Admission;
use super::owned::Owned;
use super::unix::Endpoint;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
const MAX_CONNECTIONS: usize = 16;
const MAX_SNAPSHOT: usize = super::api::MAX_SNAPSHOT_BYTES;
const PAGE: usize = super::api::PAGE_BYTES;
const SNAPSHOT_TTL: Duration = Duration::from_secs(2);
const CAPS: &[&str] = &[
    "inspect",
    "input-act-v1",
    "controller-epoch-v1",
    "paged-screen-v1",
    super::api::RETAINED_LEDGER,
    super::api::TYPED_VIEW,
    super::api::HUMAN_INPUT,
];
pub(crate) struct Server {
    endpoint: Arc<Endpoint>,
    host: Arc<Owned>,
    connections: Arc<AtomicUsize>,
    snapshots: super::view_budget::Budget,
    frames: super::view_frames::Frames,
    next_snapshot: AtomicU64,
    stop: AtomicBool,
    publication: Option<Arc<super::birth::Gate>>,
}
impl Server {
    #[cfg(test)]
    pub fn stop_fixture(&self) {
        self.stop.store(true, Ordering::Release);
    }
    pub fn new(endpoint: Endpoint, host: Arc<Owned>) -> Self {
        Self {
            endpoint: Arc::new(endpoint),
            host,
            connections: Arc::new(AtomicUsize::new(0)),
            snapshots: super::view_budget::Budget::new(),
            frames: super::view_frames::Frames::new(),
            next_snapshot: AtomicU64::new(1),
            stop: AtomicBool::new(false),
            publication: None,
        }
    }
    pub fn with_publication(
        endpoint: Endpoint,
        host: Arc<Owned>,
        publication: Arc<super::birth::Gate>,
    ) -> Self {
        let mut server = Self::new(endpoint, host);
        server.publication = Some(publication);
        server
    }
    pub fn run(self: Arc<Self>) -> io::Result<()> {
        self.endpoint.nonblocking()?;
        while !self.stop.load(Ordering::Acquire) {
            self.frames.expire(Instant::now());
            match self.endpoint.accept_stream() {
                Ok(stream) => {
                    let Some(permit) = Permit::connection(self.connections.clone()) else {
                        drop(stream);
                        continue;
                    };
                    let server = self.clone();
                    // Permit is counted before expensive handshake/thread dispatch.
                    let _ = std::thread::Builder::new()
                        .name("session-host-client".into())
                        .spawn(move || {
                            let _permit = permit;
                            let _ = server.serve(stream);
                        });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.endpoint.wait_connection(Duration::from_millis(100))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // Listener errors never destroy the stable child owner.
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        Ok(())
    }
    fn serve(self: &Arc<Self>, stream: UnixStream) -> io::Result<()> {
        let mut channel = self.endpoint.authenticate(stream)?;
        let frame = channel.receive()?;
        if frame.len() > super::wire::MAX_HELLO {
            return Err(invalid("hello limit"));
        }
        let open: Open = serde_json::from_slice(&frame).map_err(invalid)?;
        let mut capabilities = CAPS.to_vec();
        if self.publication.is_some() {
            capabilities.push(super::api::SCOPED_BIRTH);
        }
        let minor = match open.protocol.negotiate(&capabilities) {
            Ok(minor) => minor,
            Err(_) => {
                send(
                    &mut channel,
                    &Response::Error {
                        code: "unsupported_protocol_or_capability".into(),
                    },
                )?;
                return Ok(());
            }
        };
        if open.role == Role::Human
            && !open
                .protocol
                .required
                .iter()
                .any(|cap| cap == super::api::HUMAN_INPUT)
        {
            send(
                &mut channel,
                &Response::Error {
                    code: "human_input_capability_required".into(),
                },
            )?;
            return Ok(());
        }
        let identity = self.host.identity();
        if open
            .expected
            .as_ref()
            .is_some_and(|expected| expected != &identity)
            || (open.role != Role::View && open.expected.as_ref() != Some(&identity))
        {
            send(
                &mut channel,
                &Response::Error {
                    code: "wrong_host_or_session_identity".into(),
                },
            )?;
            return Ok(());
        }
        send(
            &mut channel,
            &Response::Opened {
                identity,
                minor,
                capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
                max_payload: 65_519,
            },
        )?;
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| invalid("connection entropy"))?;
        let mut connection = Connection {
            server: self.clone(),
            id: ConnectionId(u128::from_be_bytes(random)),
            role: open.role,
            fence: None,
            snapshot: None,
            stop_after_reply: false,
            latest: None,
        };
        loop {
            let bytes = channel.receive()?;
            let request: Request = serde_json::from_slice(&bytes).map_err(invalid)?;
            if connection
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| Instant::now() >= snapshot.until)
            {
                connection.snapshot = None;
            }
            let response = connection.handle(request);
            send(&mut channel, &response)?;
            if connection.stop_after_reply {
                self.stop.store(true, Ordering::Release);
                return Ok(());
            }
        }
    }
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
fn send(channel: &mut Channel<UnixStream>, response: &Response) -> io::Result<()> {
    // All response strings/data are independently bounded before this point.
    let bytes = serde_json::to_vec(response).map_err(invalid)?;
    channel.send(&bytes)
}
struct Permit {
    count: Option<Arc<AtomicUsize>>,
}
impl Permit {
    fn connection(count: Arc<AtomicUsize>) -> Option<Self> {
        count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_CONNECTIONS).then_some(n + 1)
            })
            .ok()?;
        Some(Self { count: Some(count) })
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(count) = &self.count {
            count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
struct Snapshot {
    id: u64,
    generation: u64,
    frame: Arc<super::view_frames::Frame>,
    until: Instant,
    typed: bool,
    sent: Vec<bool>,
}
struct Connection {
    server: Arc<Server>,
    id: ConnectionId,
    role: Role,
    fence: Option<Fence>,
    snapshot: Option<Snapshot>,
    stop_after_reply: bool,
    latest: Option<super::latest::Latest>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.snapshot = None;
        self.server.snapshots.forget(self.id);
        if let Some(fence) = self.fence.take() {
            let _ = self.server.host.release(fence);
        }
        if self.role == Role::Human {
            self.server.host.disconnect_human(self.id);
        }
    }
}
impl Connection {
    fn handle(&mut self, request: Request) -> Response {
        match self.try_handle(request) {
            Ok(response) => response,
            Err(code) => Response::Error { code },
        }
    }
    fn controller(&self) -> Result<Fence, String> {
        if self.role != Role::Control {
            return Err("view_connection_cannot_mutate".into());
        }
        self.fence.ok_or_else(|| "controller_required".into())
    }
    fn try_handle(&mut self, request: Request) -> Result<Response, String> {
        if self
            .server
            .publication
            .as_ref()
            .is_some_and(|gate| !gate.effects_allowed())
            && matches!(
                &request,
                Request::Acquire
                    | Request::Input { .. }
                    | Request::Resize { .. }
                    | Request::Close
                    | Request::HumanGrant { .. }
            )
        {
            return Err("host_publication_pending".into());
        }
        match request {
            Request::Inspect => {
                let mut info = self.server.host.inspect().map_err(|_| "host_unavailable")?;
                info.active_connections = self.server.connections.load(Ordering::Acquire);
                info.publication = self.server.publication.as_ref().map(|gate| gate.status());
                Ok(Response::Info { info })
            }
            Request::Acquire => {
                if self.role != Role::Control || self.fence.is_some() {
                    return Err("invalid_control_acquisition".into());
                }
                let fence = self.server.host.acquire(self.id).map_err(control_error)?;
                self.fence = Some(fence);
                Ok(Response::Granted { epoch: fence.epoch })
            }
            Request::Release => {
                let fence = self.controller()?;
                self.server.host.release(fence).map_err(control_error)?;
                self.fence = None;
                Ok(Response::Ok)
            }
            Request::Input {
                client,
                sequence,
                act,
            } => self.input(client, sequence, act),
            Request::Recover {
                client,
                sequence,
                act,
            } => {
                let (state, completion) = self
                    .server
                    .host
                    .recover(client, sequence, &act)
                    .map_err(control_error)?;
                Ok(Response::Outcome {
                    state: state.into(),
                    completion,
                })
            }
            Request::Resize { rows, cols } => {
                self.server
                    .host
                    .resize(self.controller()?, rows, cols)
                    .map_err(control_error)?;
                Ok(Response::Ok)
            }
            Request::Close => {
                self.server
                    .host
                    .close(self.controller()?)
                    .map_err(control_error)?;
                Ok(Response::Ok)
            }
            request @ (Request::Snapshot
            | Request::Viewport { .. }
            | Request::Page { .. }
            | Request::Acknowledge { .. }
            | Request::AcknowledgeViewport { .. }
            | Request::WatchAfter { .. }) => self.terminal_request(request),
            Request::Stop => self.stop(),
            request => self.human_request(request),
        }
    }
    fn human(&self) -> Result<(), String> {
        if self.role != Role::Human {
            return Err("human_role_required".into());
        }
        Ok(())
    }
    /// Raw outcomes are for the human holder and Control, never View.
    fn raw_reader(&self, grant: u64) -> Result<(), String> {
        match self.role {
            Role::Control => Ok(()),
            Role::Human if self.server.host.holds_grant(self.id, grant) => Ok(()),
            _ => Err("raw_outcome_not_visible".into()),
        }
    }
    fn human_request(&mut self, request: Request) -> Result<Response, String> {
        use super::human::GrantRequest;
        use sha2::{Digest, Sha256};
        let host = &self.server.host;
        match request {
            Request::HumanGrant {
                request,
                expected,
                recovery_secret,
            } => {
                self.human()?;
                let request = GrantRequest {
                    request,
                    expected,
                    recovery: Sha256::digest(recovery_secret).into(),
                };
                let (grant, state) = host.grant_human(self.id, request).map_err(control_error)?;
                Ok(Response::HumanGranted { grant, state })
            }
            Request::RevokeHuman { grant } => {
                let revoked = host
                    .revoke_human(self.controller()?, grant)
                    .map_err(control_error)?;
                Ok(Response::Revoked { revoked })
            }
            Request::RecoverRaw {
                grant,
                sequence,
                bytes,
            } => {
                self.raw_reader(grant)?;
                let digest = raw_digest(bytes)?;
                Ok(Response::RawOutcome {
                    state: host.recover_raw(grant, sequence, digest),
                })
            }
            Request::RawCut { last } => {
                if self.role == Role::View {
                    return Err("raw_outcome_not_visible".into());
                }
                let cut = match host.raw_cut(last) {
                    Ok(cut) => cut,
                    Err(state) => return Ok(Response::Retired { state }),
                };
                self.raw_reader(cut.grant)?;
                Ok(Response::RawCut { cut })
            }
            Request::AcknowledgeRaw { cut } => {
                self.human()?;
                Ok(Response::Retired {
                    state: host.ack_raw(self.id, cut),
                })
            }
            Request::DiscardRaw { cut } => {
                let state = host
                    .discard_raw(self.controller()?, cut)
                    .map_err(control_error)?;
                Ok(Response::Retired { state })
            }
            _ => Err("unsupported_request".into()),
        }
    }
    fn stop(&mut self) -> Result<Response, String> {
        let fence = self.controller()?;
        if self
            .server
            .publication
            .as_ref()
            .is_some_and(|gate| gate.status() != super::api::PublicationStatus::Complete)
        {
            return Err("host_publication_pending".into());
        }
        let settled = match &self.server.publication {
            Some(gate) => Some(gate.settle(&self.server.host.identity())?),
            None => None,
        };
        let info = self.server.host.inspect().map_err(|_| "host_unavailable")?;
        if !matches!(info.lifecycle, Lifecycle::Exited) || !info.output_eof {
            return Err("host_stop_requires_confirmed_exit_and_output_eof".into());
        }
        if settled.is_some_and(|proof| proof.exit_generation() > info.generation) {
            return Err("host_settlement_unavailable".into());
        }
        self.server.host.latch_stop(fence).map_err(control_error)?;
        self.stop_after_reply = true;
        Ok(Response::Ok)
    }
    fn input(
        &self,
        client: [u8; 16],
        sequence: u64,
        act: super::wire::InputAct,
    ) -> Result<Response, String> {
        let fence = self.controller()?;
        match self
            .server
            .host
            .admit(fence, client, sequence, &act)
            .map_err(control_error)?
        {
            Admission::Existing(state) => {
                let (_, completion) = self
                    .server
                    .host
                    .recover(client, sequence, &act)
                    .map_err(control_error)?;
                Ok(Response::Outcome {
                    state: state.into(),
                    completion,
                })
            }
            Admission::Started(token) => {
                let host = self.server.host.clone();
                if std::thread::Builder::new()
                    .name("session-host-act".into())
                    .spawn(move || {
                        let _ = host.execute(token);
                    })
                    .is_err()
                {
                    self.server
                        .host
                        .reject_unstarted(token)
                        .map_err(control_error)?;
                    return Err("act_worker_unavailable_rejected_before_start".into());
                }
                Ok(Response::Admitted)
            }
        }
    }
    fn terminal_request(&mut self, request: Request) -> Result<Response, String> {
        match request {
            Request::Snapshot => self.begin_snapshot(None),
            Request::Viewport { scrollback } => self.begin_snapshot(Some(scrollback)),
            Request::Page { snapshot, index } => self.page(snapshot, index),
            Request::Acknowledge {
                snapshot,
                generation,
            } => self.acknowledge(snapshot, generation, false),
            Request::AcknowledgeViewport {
                snapshot,
                generation,
            } => self.acknowledge(snapshot, generation, true),
            Request::WatchAfter {
                generation,
                timeout_ms,
            } => self
                .latest
                .as_mut()
                .ok_or("watch_requires_typed_snapshot_ack")?
                .wait(generation, timeout_ms, &self.server.host.view),
            _ => Err("invalid_terminal_request".into()),
        }
    }
    fn acknowledge(&mut self, id: u64, generation: u64, typed: bool) -> Result<Response, String> {
        if !self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.id == id
                && snapshot.generation == generation
                && snapshot.typed == typed
                && snapshot.sent.iter().all(|sent| *sent)
                && snapshot.frame.available(Instant::now())
        }) {
            return Err("expired_or_wrong_snapshot".into());
        }
        if let Some(snapshot) = self.snapshot.take() {
            snapshot.frame.clear();
        }
        if typed {
            self.latest
                .as_mut()
                .ok_or("watch_subscription_unavailable")?
                .acknowledge(generation, &self.server.host.view)
        } else {
            Ok(Response::Ok)
        }
    }
    fn begin_snapshot(&mut self, offset: Option<u16>) -> Result<Response, String> {
        if self.snapshot.is_some() {
            return Err("snapshot_ack_required".into());
        }
        if offset.is_some() && self.latest.is_none() {
            // Register before capture; no output/ACK/wait handoff gap.
            self.latest = Some(super::latest::Latest::new(&self.server.host.view)?);
        }
        let permit = self
            .server
            .snapshots
            .acquire(self.id, Instant::now())
            .ok_or("snapshot_budget_busy")?;
        let id = self
            .server
            .next_snapshot
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| "snapshot_id_exhausted")?;
        let (generation, bytes, rows, cols) = self.capture(offset)?;
        let pages = bytes.len().div_ceil(PAGE);
        let response = if offset.is_some() {
            Response::Viewport {
                identity: self.server.host.identity(),
                snapshot: id,
                generation,
                bytes: bytes.len(),
                pages,
                rows,
                cols,
            }
        } else {
            Response::Snapshot {
                snapshot: id,
                generation,
                bytes: bytes.len(),
                pages,
                rows,
                cols,
            }
        };
        let until = Instant::now() + SNAPSHOT_TTL;
        let frame = self
            .server
            .frames
            .track(bytes, permit, until)
            .ok_or("snapshot_registry_budget")?;
        self.snapshot = Some(Snapshot {
            id,
            generation,
            sent: vec![false; pages],
            frame,
            until,
            typed: offset.is_some(),
        });
        Ok(response)
    }
    fn capture(&self, offset: Option<u16>) -> Result<(u64, Vec<u8>, u16, u16), String> {
        if let Some(offset) = offset {
            let view = self
                .server
                .host
                .view
                .viewport(usize::from(offset))
                .map_err(|_| "capture_unavailable")?;
            // Expensive encoding is outside the parser lock, with an in-write cap.
            let bytes = view
                .encode()
                .map_err(|_| "typed_capture_budget_or_bounds")?;
            Ok((view.generation, bytes, view.rows, view.cols))
        } else {
            let (generation, bytes, (rows, cols)) = self
                .server
                .host
                .view
                .capture()
                .map_err(|_| "capture_unavailable")?;
            if bytes.len() > MAX_SNAPSHOT {
                return Err("snapshot_byte_limit".into());
            }
            Ok((generation, bytes, rows, cols))
        }
    }
    fn page(&mut self, id: u64, index: usize) -> Result<Response, String> {
        let snapshot = self
            .snapshot
            .as_mut()
            .filter(|s| s.id == id)
            .ok_or("snapshot_expired")?;
        let base64 = snapshot
            .frame
            .page(index, Instant::now())
            .ok_or("invalid_or_expired_page")?;
        snapshot.sent[index] = true;
        Ok(Response::Page {
            snapshot: id,
            index,
            base64,
        })
    }
}
/// Raw bytes are digested as a typed Bytes act, binding operation kind.
fn raw_digest(bytes: Vec<u8>) -> Result<[u8; 32], String> {
    super::wire::InputAct::Bytes(bytes)
        .digest()
        .map_err(|_| "invalid_raw_input".into())
}
fn control_error(error: super::coordinator::Error) -> String {
    use super::control::Refusal;
    use super::coordinator::Error;
    match error {
        Error::Control(Refusal::Busy) => "controller_busy",
        Error::Control(Refusal::Stale) | Error::StaleCompletion => "stale_controller_or_act",
        Error::Control(Refusal::Exhausted) => "host_sequence_exhausted",
        Error::Invalid(_) => "invalid_or_unavailable_operation",
        Error::Stopping => "host_stopping",
        Error::Retained => "host_raw_retention_pending",
        Error::Grant(super::human::GrantError::Stale) => "human_grant_stale",
        Error::Grant(super::human::GrantError::Conflict) => "human_grant_conflict",
        Error::Grant(super::human::GrantError::Exhausted) => "human_grant_exhausted",
    }
    .into()
}

#[cfg(test)]
#[path = "human_wire_tests.rs"]
mod human_wire_tests;
