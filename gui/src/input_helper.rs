//! Bounded, single-flight subprocess bridge for instance-bound input.
//! Selection/focus currentness is the caller's responsibility.
use remuda_native::strict_input::{
    InputResult, InputTransport, LocalInput, PeerInput, StrictInput, MAX_BATCH,
};
use std::{
    io::{Read, Write},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

const DEADLINE: Duration = Duration::from_secs(8);
const READY: &[u8] = b"remuda-input-v2\n";

// One watchdog bounds startup and any partially read frame or active write.
// An idle helper waits for a first byte and exits on parent pipe closure.
fn watchdog() -> mpsc::Sender<bool> {
    let (watch, deadlines) = mpsc::channel::<bool>();
    std::thread::spawn(move || loop {
        match deadlines.recv() {
            Ok(true) => {
                if deadlines.recv_timeout(DEADLINE).is_err() {
                    std::process::exit(124);
                }
            }
            _ => return,
        }
    });
    watch
}
fn serve<T: InputTransport>(mut input: StrictInput<T>, watch: mpsc::Sender<bool>) -> i32 {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    if stdout
        .write_all(READY)
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return 1;
    }
    let _ = watch.send(false);
    loop {
        let mut header = [0; 4];
        if stdin.read_exact(&mut header[..1]).is_err() {
            return 0;
        }
        if watch.send(true).is_err() {
            return 1;
        }
        if stdin.read_exact(&mut header[1..]).is_err() {
            return 1;
        }
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > MAX_BATCH {
            return 1;
        }
        let mut bytes = vec![0; length];
        if stdin.read_exact(&mut bytes).is_err() {
            return 1;
        }
        let result = input.send(&bytes);
        let ack = u8::from(result == InputResult::Applied);
        if stdout
            .write_all(&[ack])
            .and_then(|()| stdout.flush())
            .is_err()
            || ack == 0
        {
            return 1;
        }
        let _ = watch.send(false);
    }
}

// Peer lines use the guarded remote variant. A peer must prove support
// before any user bytes are sent; an ambiguous outcome is never retried.
struct GuardedPeer<T> {
    transport: T,
    supported: bool,
}
impl<T: InputTransport> InputTransport for GuardedPeer<T> {
    fn send(
        &mut self,
        request: &remuda_core::protocol::Request,
    ) -> std::io::Result<remuda_core::protocol::Response> {
        use remuda_core::protocol::{Request, Response};
        if !self.supported {
            if !matches!(
                self.transport.send(&Request::RemoteInputSupport)?,
                Response::Ok
            ) {
                return Err(std::io::Error::other("peer does not support guarded input"));
            }
            self.supported = true;
        }
        let Request::Input {
            name,
            instance_id,
            client_id,
            seq,
            bytes,
        } = request
        else {
            return Err(std::io::Error::other("unexpected input request"));
        };
        self.transport.send(&Request::RemoteInputLine {
            name: name.clone(),
            instance_id: instance_id.clone(),
            client_id: client_id.clone(),
            seq: *seq,
            bytes: bytes.clone(),
        })
    }
}

pub fn worker(args: &[String]) -> i32 {
    if args.len() != 4 || args[2].is_empty() || args[3].is_empty() {
        return 1;
    }
    let watch = watchdog();
    let _ = watch.send(true);
    let name = args[2].clone();
    let instance = args[3].clone();
    match args[0].as_str() {
        "local" if args[1].is_empty() => StrictInput::new(
            LocalInput(remuda_native::daemon::socket_path("default")),
            name,
            instance,
        )
        .map_or(1, |input| serve(input, watch)),
        "peer" if !args[1].is_empty() => PeerInput::new(args[1].clone())
            .and_then(|peer| {
                StrictInput::new(
                    GuardedPeer {
                        transport: peer,
                        supported: false,
                    },
                    name,
                    instance,
                )
            })
            .map_or(1, |input| serve(input, watch)),
        _ => 1,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Applied,
    NotAcknowledged,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Submit {
    Queued,
    Busy,
    Invalid,
    Stopped,
}
struct State {
    stopped: bool,
    busy: bool,
    completed: bool,
}

/// One helper and one outstanding batch, with no further buffered input.
/// `stop` prevents future dispatch; bytes already handed to the pipe may land.
/// Events never contain input bytes. Drop requests stop and cleanup is asynchronous.
pub struct InputHelper {
    tx: mpsc::SyncSender<Vec<u8>>,
    state: Arc<Mutex<State>>,
    event_tx: mpsc::SyncSender<Event>,
    events: mpsc::Receiver<Event>,
}

impl InputHelper {
    pub fn start(program: &std::path::Path, args: &[&str]) -> std::io::Result<Self> {
        use std::process::{Command, Stdio};
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let (events, results) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(State {
            stopped: false,
            busy: false,
            completed: false,
        }));
        let worker_state = state.clone();
        let worker_events = events.clone();
        std::thread::spawn(move || {
            let mut ready = false;
            for bytes in rx {
                {
                    let s = worker_state.lock().unwrap();
                    if s.stopped {
                        break;
                    }
                }
                let result = exchange(&mut stdin, &mut stdout, &bytes, &worker_state, &mut ready);
                let mut s = worker_state.lock().unwrap();
                if s.busy && !s.completed {
                    s.completed = true;
                    if !result {
                        s.stopped = true;
                    }
                    let _ = worker_events.try_send(if result {
                        Event::Applied
                    } else {
                        Event::NotAcknowledged
                    });
                }
                if s.stopped {
                    break;
                }
            }
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
        });
        Ok(Self {
            tx,
            state,
            event_tx: events,
            events: results,
        })
    }
    pub fn submit(&self, bytes: &[u8]) -> Submit {
        if bytes.is_empty() || bytes.len() > MAX_BATCH {
            return Submit::Invalid;
        }
        let mut state = self.state.lock().unwrap();
        if state.stopped {
            return Submit::Stopped;
        }
        if state.busy {
            return Submit::Busy;
        }
        state.busy = true;
        if self.tx.try_send(bytes.to_vec()).is_err() {
            state.stopped = true;
            return Submit::Stopped;
        }
        Submit::Queued
    }
    /// Consume the single completion before another batch can be submitted.
    pub fn recv_event_timeout(&self, timeout: Duration) -> Result<Event, mpsc::RecvTimeoutError> {
        let event = self.events.recv_timeout(timeout)?;
        let mut state = self.state.lock().unwrap();
        state.busy = false;
        state.completed = false;
        Ok(event)
    }
    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopped = true;
        if state.busy && !state.completed {
            state.completed = true;
            let _ = self.event_tx.try_send(Event::NotAcknowledged);
        }
        drop(state);
        // Wake a worker waiting for its next batch.
        let _ = self.tx.try_send(Vec::new());
    }
}
impl Drop for InputHelper {
    fn drop(&mut self) {
        self.stop();
    }
}

// The descriptor is nonblocking. Holding this lock only for the actual write
// orders every accepted byte before stop() returns; never sleep under it.
#[cfg(unix)]
fn guarded_write(
    state: &Mutex<State>,
    write: impl FnOnce() -> std::io::Result<usize>,
) -> Option<std::io::Result<usize>> {
    let guard = state.lock().unwrap();
    if guard.stopped {
        None
    } else {
        Some(write())
    }
}

#[cfg(unix)]
fn exchange(
    input: &mut std::process::ChildStdin,
    output: &mut std::process::ChildStdout,
    bytes: &[u8],
    state: &Mutex<State>,
    ready: &mut bool,
) -> bool {
    use std::os::fd::AsRawFd;
    let fds = [input.as_raw_fd(), output.as_raw_fd()];
    for fd in fds {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return false;
            }
        }
    }
    let end = Instant::now() + DEADLINE;
    let mut received = 0;
    while !*ready {
        if state.lock().unwrap().stopped || Instant::now() >= end {
            return false;
        }
        let mut buf = [0; 1];
        match output.read(&mut buf) {
            Ok(1) if buf[0] == READY[received] => {
                received += 1;
                *ready = received == READY.len();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => return false,
        }
    }
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(bytes);
    let mut written = 0;
    while written < frame.len() {
        if Instant::now() >= end {
            return false;
        }
        match guarded_write(state, || input.write(&frame[written..])) {
            None => return false,
            Some(Ok(0)) => return false,
            Some(Ok(n)) => written += n,
            Some(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Some(Err(_)) => return false,
        }
    }
    loop {
        if state.lock().unwrap().stopped || Instant::now() >= end {
            return false;
        }
        let mut ack = [0];
        match output.read(&mut ack) {
            Ok(1) => return ack[0] == 1,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => return false,
        }
    }
}
#[cfg(not(unix))]
fn exchange(
    _: &mut std::process::ChildStdin,
    _: &mut std::process::ChildStdout,
    _: &[u8],
    _: &Mutex<State>,
    _: &mut bool,
) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn guarded_peer_checks_support_before_sending_instance_bound_line() {
        use remuda_core::protocol::{Request, Response};
        struct Fake {
            seen: Arc<Mutex<Vec<Request>>>,
            support: bool,
        }
        impl InputTransport for Fake {
            fn send(&mut self, request: &Request) -> std::io::Result<Response> {
                self.seen.lock().unwrap().push(request.clone());
                match request {
                    Request::RemoteInputSupport if self.support => Ok(Response::Ok),
                    Request::RemoteInputSupport => Err(std::io::Error::other("unsupported")),
                    Request::RemoteInputLine { .. } => Ok(Response::Ack { duplicate: false }),
                    _ => panic!("unexpected request"),
                }
            }
        }
        for supported in [false, true] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let transport = GuardedPeer {
                transport: Fake {
                    seen: seen.clone(),
                    support: supported,
                },
                supported: false,
            };
            let mut input =
                StrictInput::new(transport, "peer-session".into(), "instance-1".into()).unwrap();
            let result = input.send(b"test\r");
            let seen = seen.lock().unwrap();
            assert!(matches!(seen.first(), Some(Request::RemoteInputSupport)));
            if supported {
                assert_eq!(result, InputResult::Applied);
                assert!(
                    matches!(&seen[1], Request::RemoteInputLine { name, instance_id, bytes, .. } if name == "peer-session" && instance_id == "instance-1" && bytes == b"test\r")
                );
            } else {
                assert_eq!(result, InputResult::NotAcknowledged);
                assert_eq!(seen.len(), 1, "unsupported peer must see no user input");
            }
        }
    }
    #[test]
    fn unconsumed_completion_blocks_further_batches() {
        let helper = InputHelper::start(
            Path::new("/bin/sh"),
            &[
                "-c",
                "printf 'remuda-input-v2\\n'; dd bs=1 count=5 of=/dev/null 2>/dev/null; printf '\\001'",
            ],
        )
        .unwrap();
        assert_eq!(helper.submit(b"x"), Submit::Queued);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !helper.state.lock().unwrap().completed {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        for _ in 0..1000 {
            assert_eq!(helper.submit(b"y"), Submit::Busy);
        }
        assert_eq!(
            helper.recv_event_timeout(Duration::ZERO).unwrap(),
            Event::Applied
        );
    }
    #[test]
    fn stopped_queued_batch_has_exactly_one_completion() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let (event_tx, events) = mpsc::sync_channel(1);
        let helper = InputHelper {
            tx,
            state: Arc::new(Mutex::new(State {
                stopped: false,
                busy: false,
                completed: false,
            })),
            event_tx,
            events,
        };
        assert_eq!(helper.submit(b"x"), Submit::Queued);
        helper.stop();
        helper.stop();
        assert_eq!(
            helper.recv_event_timeout(Duration::ZERO).unwrap(),
            Event::NotAcknowledged
        );
        assert!(helper.recv_event_timeout(Duration::ZERO).is_err());
        assert_eq!(helper.submit(b"y"), Submit::Stopped);
    }
    #[test]
    fn stop_between_prior_check_and_write_prevents_pipe_handoff() {
        let helper = InputHelper::start(Path::new("/bin/sleep"), &["30"]).unwrap();
        let state = helper.state.clone();
        let (checked_tx, checked_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            assert!(!state.lock().unwrap().stopped);
            checked_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            guarded_write(&state, || panic!("write after stop"))
        });
        checked_rx.recv().unwrap();
        helper.stop();
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_none());
    }
    #[test]
    fn serial_ack_and_invalid_batch() {
        let helper = InputHelper::start(Path::new("/bin/sh"), &["-c", "printf 'remuda-input-v2\\n'; dd bs=1 count=5 of=/dev/null 2>/dev/null; printf '\\001'; dd bs=1 count=5 of=/dev/null 2>/dev/null; printf '\\001'"]).unwrap();
        assert_eq!(helper.submit(&[]), Submit::Invalid);
        assert_eq!(helper.submit(&vec![0; MAX_BATCH + 1]), Submit::Invalid);
        for _ in 0..2 {
            assert_eq!(helper.submit(b"x"), Submit::Queued);
            assert_eq!(
                helper.recv_event_timeout(Duration::from_secs(3)).unwrap(),
                Event::Applied
            );
        }
    }
    #[test]
    fn stalled_child_is_bounded_and_stop_discards_further_input() {
        let helper = InputHelper::start(Path::new("/bin/sleep"), &["30"]).unwrap();
        assert_eq!(helper.submit(b"x"), Submit::Queued);
        assert_eq!(helper.submit(b"y"), Submit::Busy);
        helper.stop();
        assert_eq!(helper.submit(b"z"), Submit::Stopped);
        // Depending on the stop/dispatch race, work is discarded or its outcome
        // is uncertain; no second input may be sent in either case.
        assert_eq!(
            helper
                .recv_event_timeout(Duration::from_millis(500))
                .unwrap(),
            Event::NotAcknowledged
        );
        assert!(helper
            .recv_event_timeout(Duration::from_millis(100))
            .is_err());
    }
    #[test]
    fn old_v1_worker_receives_no_input_after_peer_protocol_change() {
        let sink = std::env::temp_dir().join(format!("remuda-v1-sink-{}", std::process::id()));
        let marker = sink.with_extension("started");
        let _ = std::fs::remove_file(&sink);
        let _ = std::fs::remove_file(&marker);
        let helper = InputHelper::start(
            Path::new("/bin/sh"),
            &[
                "-c",
                "printf started > \"$2\"; printf 'remuda-input-v1\\n'; dd bs=1 count=1 of=\"$1\" 2>/dev/null",
                "sh",
                sink.to_str().unwrap(),
                marker.to_str().unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(helper.submit(b"private payload"), Submit::Queued);
        assert_eq!(
            helper
                .recv_event_timeout(DEADLINE + Duration::from_secs(3))
                .unwrap(),
            Event::NotAcknowledged
        );
        drop(helper);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(std::fs::read(&marker).unwrap(), b"started");
        assert_eq!(std::fs::read(&sink).unwrap_or_default(), b"");
        let _ = std::fs::remove_file(sink);
        let _ = std::fs::remove_file(marker);
    }
    #[test]
    fn legacy_worker_receives_no_input_before_ready() {
        let sink = std::env::temp_dir().join(format!("remuda-legacy-sink-{}", std::process::id()));
        let _ = std::fs::remove_file(&sink);
        let helper = InputHelper::start(
            Path::new("/bin/sh"),
            &[
                "-c",
                "dd bs=1 count=1 of=\"$1\" 2>/dev/null; printf '\\001'",
                "sh",
                sink.to_str().unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(helper.submit(b"private payload"), Submit::Queued);
        assert_eq!(
            helper
                .recv_event_timeout(DEADLINE + Duration::from_secs(3))
                .unwrap(),
            Event::NotAcknowledged
        );
        assert_eq!(helper.submit(b"again"), Submit::Stopped);
        drop(helper);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(std::fs::read(&sink).unwrap_or_default(), b"");
        let _ = std::fs::remove_file(sink);
    }
    #[test]
    #[ignore = "requires a separately built pre-fence GUI binary"]
    fn actual_legacy_input_worker_receives_no_payload() {
        let old = std::env::var("REMUDA_GUI_OLD_BINARY").expect("old binary path");
        let root = std::env::temp_dir().join(format!("remuda-old-input-{}", std::process::id()));
        std::fs::create_dir_all(root.join("remuda")).unwrap();
        let socket = root.join("remuda/default.sock");
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let runtime = format!("REMUDA_RUNTIME_DIR={}", root.display());
        let helper = InputHelper::start(
            Path::new("/usr/bin/env"),
            &[
                &runtime,
                &old,
                "--internal-input-worker",
                "local",
                "",
                "session",
                "instance",
            ],
        )
        .unwrap();
        assert_eq!(helper.submit(b"private payload"), Submit::Queued);
        assert_eq!(
            helper
                .recv_event_timeout(DEADLINE + Duration::from_secs(3))
                .unwrap(),
            Event::NotAcknowledged
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(helper);
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn timed_out_transport_stops_without_retry() {
        let helper = InputHelper::start(Path::new("/bin/sleep"), &["30"]).unwrap();
        assert_eq!(helper.submit(b"x"), Submit::Queued);
        assert_eq!(
            helper
                .recv_event_timeout(DEADLINE + Duration::from_secs(3))
                .unwrap(),
            Event::NotAcknowledged
        );
        assert_eq!(helper.submit(b"x"), Submit::Stopped);
    }
    #[test]
    #[ignore = "child fixture for watchdog"]
    fn stalled_transport_fixture() {
        struct Stall;
        impl InputTransport for Stall {
            fn send(
                &mut self,
                _: &remuda_core::protocol::Request,
            ) -> std::io::Result<remuda_core::protocol::Response> {
                std::thread::sleep(Duration::from_secs(30));
                unreachable!()
            }
        }
        let sender = StrictInput::new(Stall, "s".into(), "i".into()).unwrap();
        let watch = watchdog();
        let _ = watch.send(true);
        std::process::exit(serve(sender, watch));
    }
    #[test]
    #[ignore = "child fixture for startup deadline"]
    fn stalled_startup_fixture() {
        let watch = watchdog();
        watch.send(true).unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }
    fn watchdog_child(fixture: &str, partial: bool) {
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", fixture])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        if partial {
            input.write_all(&[0]).unwrap();
            input.flush().unwrap();
        }
        let deadline = Instant::now() + DEADLINE + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("watchdog did not terminate helper");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(124));
    }
    #[test]
    fn startup_deadline_exits() {
        watchdog_child("input_helper::tests::stalled_startup_fixture", false);
    }
    #[test]
    fn partial_frame_deadline_exits() {
        watchdog_child("input_helper::tests::stalled_transport_fixture", true);
    }
    #[test]
    fn helper_watchdog_exits_after_parent_pipe_closes_during_stall() {
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "input_helper::tests::stalled_transport_fixture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(&1u32.to_be_bytes()).unwrap();
        input.write_all(b"x").unwrap();
        drop(input);
        let deadline = Instant::now() + DEADLINE + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("watchdog did not terminate helper");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(124));
    }
    #[test]
    fn failed_child_is_terminal_and_never_retried() {
        let helper = InputHelper::start(Path::new("/bin/sh"), &["-c", "exit 1"]).unwrap();
        assert_eq!(helper.submit(b"x"), Submit::Queued);
        assert_eq!(
            helper.recv_event_timeout(Duration::from_secs(3)).unwrap(),
            Event::NotAcknowledged
        );
        assert_eq!(helper.submit(b"x"), Submit::Stopped);
    }
}
