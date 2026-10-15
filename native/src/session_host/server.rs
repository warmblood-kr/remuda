//! Experimental single-session server. No daemon integration or discovery index.
use super::api::{Lifecycle, Open, Request, Response, Role};
use super::channel::Channel;
use super::control::{ConnectionId, Fence};
use super::coordinator::Admission;
use super::owned::Owned;
use super::unix::Endpoint;
use base64::Engine;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
const MAX_CONNECTIONS: usize = 16;
const MAX_SNAPSHOT: usize = 32 * 1024 * 1024;
const PAGE: usize = 12 * 1024;
const SNAPSHOT_TTL: Duration = Duration::from_secs(2);
const CAPS: &[&str] = &[
    "inspect",
    "input-act-v1",
    "controller-epoch-v1",
    "paged-screen-v1",
];
pub(crate) struct Server {
    endpoint: Arc<Endpoint>,
    host: Arc<Owned>,
    connections: Arc<AtomicUsize>,
    snapshot_busy: Arc<AtomicBool>,
    next_snapshot: AtomicU64,
    stop: AtomicBool,
}
impl Server {
    pub fn new(endpoint: Endpoint, host: Arc<Owned>) -> Self {
        Self {
            endpoint: Arc::new(endpoint),
            host,
            connections: Arc::new(AtomicUsize::new(0)),
            snapshot_busy: Arc::new(AtomicBool::new(false)),
            next_snapshot: AtomicU64::new(1),
            stop: AtomicBool::new(false),
        }
    }
    pub fn run(self: Arc<Self>) -> io::Result<()> {
        self.endpoint.nonblocking()?;
        while !self.stop.load(Ordering::Acquire) {
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
        let minor = match open.protocol.negotiate(CAPS) {
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
        let identity = self.host.identity();
        if open
            .expected
            .as_ref()
            .is_some_and(|expected| expected != &identity)
            || (open.role == Role::Control && open.expected.as_ref() != Some(&identity))
        {
            send(
                &mut channel,
                &Response::Error {
                    code: "wrong_host_or_session_identity".into(),
                },
            )?;
            return Ok(());
        }
        send(&mut channel, &Response::Opened { identity, minor })?;
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| invalid("connection entropy"))?;
        let mut connection = Connection {
            server: self.clone(),
            id: ConnectionId(u128::from_be_bytes(random)),
            role: open.role,
            fence: None,
            snapshot: None,
            stop_after_reply: false,
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
    snapshot: Option<Arc<AtomicBool>>,
}
impl Permit {
    fn connection(count: Arc<AtomicUsize>) -> Option<Self> {
        count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_CONNECTIONS).then_some(n + 1)
            })
            .ok()?;
        Some(Self {
            count: Some(count),
            snapshot: None,
        })
    }
    fn snapshot(busy: Arc<AtomicBool>) -> Option<Self> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Self {
            count: None,
            snapshot: Some(busy),
        })
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(count) = &self.count {
            count.fetch_sub(1, Ordering::AcqRel);
        }
        if let Some(busy) = &self.snapshot {
            busy.store(false, Ordering::Release);
        }
    }
}
struct Snapshot {
    id: u64,
    generation: u64,
    bytes: Vec<u8>,
    until: Instant,
    _permit: Permit,
    sent: Vec<bool>,
}
struct Connection {
    server: Arc<Server>,
    id: ConnectionId,
    role: Role,
    fence: Option<Fence>,
    snapshot: Option<Snapshot>,
    stop_after_reply: bool,
}
impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(fence) = self.fence.take() {
            let _ = self.server.host.release(fence);
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
        match request {
            Request::Inspect => {
                let mut info = self.server.host.inspect().map_err(|_| "host_unavailable")?;
                info.active_connections = self.server.connections.load(Ordering::Acquire);
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
            Request::Snapshot => self.begin_snapshot(),
            Request::Page { snapshot, index } => self.page(snapshot, index),
            Request::Acknowledge {
                snapshot,
                generation,
            } => {
                if !self.snapshot.as_ref().is_some_and(|s| {
                    s.id == snapshot
                        && s.generation == generation
                        && s.sent.iter().all(|sent| *sent)
                }) {
                    return Err("expired_or_wrong_snapshot".into());
                }
                self.snapshot = None;
                Ok(Response::Ok)
            }
            Request::Stop => {
                self.controller()?;
                let info = self.server.host.inspect().map_err(|_| "host_unavailable")?;
                if !matches!(info.lifecycle, Lifecycle::Exited) || !info.output_eof {
                    return Err("host_stop_requires_confirmed_exit_and_output_eof".into());
                }
                self.server
                    .host
                    .ensure_quiescent(self.controller()?)
                    .map_err(control_error)?;
                self.stop_after_reply = true;
                Ok(Response::Ok)
            }
        }
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
    fn begin_snapshot(&mut self) -> Result<Response, String> {
        if self.snapshot.is_some() {
            return Err("snapshot_ack_required".into());
        }
        let permit =
            Permit::snapshot(self.server.snapshot_busy.clone()).ok_or("snapshot_budget_busy")?;
        let id = self
            .server
            .next_snapshot
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| "snapshot_id_exhausted")?;
        let (generation, bytes, (rows, cols)) = self
            .server
            .host
            .view
            .capture()
            .map_err(|_| "capture_unavailable")?;
        if bytes.len() > MAX_SNAPSHOT {
            return Err("snapshot_byte_limit".into());
        }
        let response = Response::Snapshot {
            snapshot: id,
            generation,
            bytes: bytes.len(),
            pages: bytes.len().div_ceil(PAGE),
            rows,
            cols,
        };
        self.snapshot = Some(Snapshot {
            id,
            generation,
            sent: vec![false; bytes.len().div_ceil(PAGE)],
            bytes,
            until: Instant::now() + SNAPSHOT_TTL,
            _permit: permit,
        });
        Ok(response)
    }
    fn page(&mut self, id: u64, index: usize) -> Result<Response, String> {
        let snapshot = self
            .snapshot
            .as_mut()
            .filter(|s| s.id == id)
            .ok_or("snapshot_expired")?;
        let start = index
            .checked_mul(PAGE)
            .filter(|start| *start < snapshot.bytes.len())
            .ok_or("invalid_page")?;
        let end = snapshot.bytes.len().min(start + PAGE);
        snapshot.sent[index] = true;
        Ok(Response::Page {
            snapshot: id,
            index,
            base64: base64::engine::general_purpose::STANDARD.encode(&snapshot.bytes[start..end]),
        })
    }
}
fn control_error(error: super::coordinator::Error) -> String {
    use super::control::Refusal;
    use super::coordinator::Error;
    match error {
        Error::Control(Refusal::Busy) => "controller_busy",
        Error::Control(Refusal::Stale) | Error::StaleCompletion => "stale_controller_or_act",
        Error::Control(Refusal::Exhausted) => "host_sequence_exhausted",
        Error::Invalid(_) => "invalid_or_unavailable_operation",
    }
    .into()
}
