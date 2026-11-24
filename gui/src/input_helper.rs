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
// The frame flag only changes the local worker's input mode. An older worker
// refuses this as an oversized frame before handing bytes to the daemon.
const PASTE_IMAGE: u32 = 1 << 31;
const READY: &[u8] = b"remuda-input-v1\n";

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
fn serve<T: InputTransport>(
    mut input: StrictInput<T>,
    watch: mpsc::Sender<bool>,
    line: Option<bool>,
    local_session: Option<&str>,
) -> i32 {
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
        let header = u32::from_be_bytes(header);
        let paste_image = header & PASTE_IMAGE != 0;
        let length = (header & !PASTE_IMAGE) as usize;
        if length == 0 || length > MAX_BATCH {
            return 1;
        }
        let mut bytes = vec![0; length];
        if stdin.read_exact(&mut bytes).is_err() {
            return 1;
        }
        let result = if paste_image {
            let Some(bytes) = local_session
                .filter(|name| bracketed_paste_enabled(name))
                .and_then(|_| image_paste_bytes(&bytes))
            else {
                if stdout
                    .write_all(&[2])
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    return 1;
                }
                let _ = watch.send(false);
                continue;
            };
            input.send(&bytes)
        } else {
            match line {
                Some(remote) => input.send_line(&bytes, remote),
                None => input.send(&bytes),
            }
        };
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

fn bracketed_paste_enabled(name: &str) -> bool {
    use remuda_core::protocol::{Request, Response};
    LocalInput(remuda_native::daemon::socket_path("default"))
        .send(&Request::MouseState { name: name.into() })
        .is_ok_and(|reply| matches!(reply, Response::MouseState(state) if state.bracketed_paste))
}

fn image_paste_bytes(path: &[u8]) -> Option<Vec<u8>> {
    let path = std::str::from_utf8(path).ok()?;
    if !std::path::Path::new(path).is_absolute()
        || path.chars().any(char::is_control)
        || path.contains('"')
        || path.len() > MAX_BATCH - 14
    {
        return None;
    }
    if path.chars().any(char::is_whitespace) {
        Some([b"\x1b[200~\"", path.as_bytes(), b"\"\x1b[201~"].concat())
    } else {
        Some([b"\x1b[200~", path.as_bytes(), b"\x1b[201~"].concat())
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
        "local" | "line-local" if args[1].is_empty() => StrictInput::new(
            LocalInput(remuda_native::daemon::socket_path("default")),
            name.clone(),
            instance,
        )
        .map_or(1, |input| {
            serve(
                input,
                watch,
                (args[0] == "line-local").then_some(false),
                Some(&name),
            )
        }),
        "peer" | "line-peer" if !args[1].is_empty() => PeerInput::new(args[1].clone())
            .and_then(|peer| StrictInput::new(peer, name, instance))
            .map_or(1, |input| {
                serve(input, watch, (args[0] == "line-peer").then_some(true), None)
            }),
        _ => 1,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Applied,
    NotAcknowledged,
    PasteUnavailable,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Submit {
    Queued,
    Busy,
    Invalid,
    Stopped,
}
#[derive(Default)]
struct State {
    stopped: bool,
    busy: bool,
    completed: bool,
    #[cfg(windows)]
    started: Option<Instant>,
}

impl State {
    fn complete(&mut self, result: Event, events: &mpsc::SyncSender<Event>) {
        if self.busy && !self.completed {
            self.completed = true;
            let _ = events.try_send(result);
        }
        if result == Event::NotAcknowledged {
            self.stopped = true;
        }
    }
}

/// One helper and one outstanding batch, with no further buffered input.
/// `stop` prevents new dispatch; an in-flight batch may still land.
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
        let program = program.to_owned();
        let args: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let (events, results) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(State::default()));
        let worker_state = state.clone();
        let worker_events = events.clone();
        std::thread::Builder::new()
            .name("gui-input".into())
            .spawn(move || {
                if worker_state.lock().unwrap().stopped {
                    return;
                }
                let mut command = Command::new(program);
                command
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
                    command.creation_flags(CREATE_NO_WINDOW);
                }
                let Ok(mut child) = command.spawn() else {
                    worker_state
                        .lock()
                        .unwrap()
                        .complete(Event::NotAcknowledged, &worker_events);
                    return;
                };
                let mut stdin = child.stdin.take().unwrap();
                let mut stdout = child.stdout.take().unwrap();
                #[cfg(windows)]
                let child_watch = {
                    let state = worker_state.clone();
                    std::thread::spawn(move || watch_windows_child(child, &state))
                };
                let mut ready = false;
                for bytes in rx {
                    {
                        let s = worker_state.lock().unwrap();
                        if s.stopped {
                            break;
                        }
                    }
                    let result =
                        exchange(&mut stdin, &mut stdout, &bytes, &worker_state, &mut ready);
                    let mut s = worker_state.lock().unwrap();
                    s.complete(result, &worker_events);
                    if s.stopped {
                        break;
                    }
                }
                drop(stdin);
                #[cfg(not(windows))]
                {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                #[cfg(windows)]
                {
                    worker_state.lock().unwrap().stopped = true;
                    let _ = child_watch.join();
                }
            })?;
        Ok(Self {
            tx,
            state,
            event_tx: events,
            events: results,
        })
    }
    pub fn submit(&self, bytes: &[u8]) -> Submit {
        self.submit_frame(bytes, 0)
    }

    pub(crate) fn paste_image_path(&self, path: &str) -> Submit {
        self.submit_frame(path.as_bytes(), PASTE_IMAGE)
    }

    fn submit_frame(&self, bytes: &[u8], flags: u32) -> Submit {
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
        #[cfg(windows)]
        {
            state.started = Some(Instant::now());
        }
        let mut frame = Vec::with_capacity(4 + bytes.len());
        frame.extend_from_slice(&((bytes.len() as u32) | flags).to_be_bytes());
        frame.extend_from_slice(bytes);
        if self.tx.try_send(frame).is_err() {
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
    frame: &[u8],
    state: &Mutex<State>,
    ready: &mut bool,
) -> Event {
    use std::os::fd::AsRawFd;
    let fds = [input.as_raw_fd(), output.as_raw_fd()];
    for fd in fds {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Event::NotAcknowledged;
            }
        }
    }
    let end = Instant::now() + DEADLINE;
    let mut received = 0;
    while !*ready {
        if state.lock().unwrap().stopped || Instant::now() >= end {
            return Event::NotAcknowledged;
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
            _ => return Event::NotAcknowledged,
        }
    }
    let mut written = 0;
    while written < frame.len() {
        if Instant::now() >= end {
            return Event::NotAcknowledged;
        }
        match guarded_write(state, || input.write(&frame[written..])) {
            None => return Event::NotAcknowledged,
            Some(Ok(0)) => return Event::NotAcknowledged,
            Some(Ok(n)) => written += n,
            Some(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Some(Err(_)) => return Event::NotAcknowledged,
        }
    }
    loop {
        if state.lock().unwrap().stopped || Instant::now() >= end {
            return Event::NotAcknowledged;
        }
        let mut ack = [0];
        match output.read(&mut ack) {
            Ok(1) => return decode_ack(ack[0]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => return Event::NotAcknowledged,
        }
    }
}
#[cfg(windows)]
fn watch_windows_child(mut child: std::process::Child, state: &Mutex<State>) {
    loop {
        let stop = {
            let state = state.lock().unwrap();
            state.stopped
                || (state.busy
                    && !state.completed
                    && state
                        .started
                        .is_some_and(|start| start.elapsed() >= DEADLINE))
        };
        if stop {
            let _ = child.kill();
            break;
        }
        if !matches!(child.try_wait(), Ok(None)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.wait();
}

// Windows subprocess pipes use blocking I/O; a separate watcher owns the
// child's lifetime so a stalled worker releases both pipes. The currentness
// check commits the single batch to this exact helper before detach can win.
#[cfg(windows)]
fn exchange(
    input: &mut std::process::ChildStdin,
    output: &mut std::process::ChildStdout,
    frame: &[u8],
    state: &Mutex<State>,
    ready: &mut bool,
) -> Event {
    if !*ready {
        let mut startup = [0; READY.len()];
        if output.read_exact(&mut startup).is_err() || startup != READY {
            return Event::NotAcknowledged;
        }
        *ready = true;
    }
    if state.lock().unwrap().stopped {
        return Event::NotAcknowledged;
    }
    let mut ack = [0];
    if input.write_all(frame).is_ok() && output.read_exact(&mut ack).is_ok() {
        decode_ack(ack[0])
    } else {
        Event::NotAcknowledged
    }
}

#[cfg(not(any(unix, windows)))]
fn exchange(
    _: &mut std::process::ChildStdin,
    _: &mut std::process::ChildStdout,
    _: &[u8],
    _: &Mutex<State>,
    _: &mut bool,
) -> Event {
    Event::NotAcknowledged
}

fn decode_ack(ack: u8) -> Event {
    match ack {
        1 => Event::Applied,
        2 => Event::PasteUnavailable,
        _ => Event::NotAcknowledged,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn image_paste_sends_one_bracketed_path_and_no_return() {
        assert_eq!(
            image_paste_bytes(b"/tmp/screenshot.png").unwrap(),
            b"\x1b[200~/tmp/screenshot.png\x1b[201~"
        );
        assert_eq!(
            image_paste_bytes(b"/tmp/screen shot.png").unwrap(),
            b"\x1b[200~\"/tmp/screen shot.png\"\x1b[201~"
        );
        assert!(image_paste_bytes(b"screenshot.png").is_none());
        assert!(image_paste_bytes(b"/tmp/screenshot.png\r").is_none());
        assert!(image_paste_bytes(b"/tmp/screenshot.png\x1b[201~").is_none());
    }

    #[test]
    fn paste_refusal_does_not_stop_the_input_helper() {
        let helper = InputHelper::start(
            Path::new("/bin/sh"),
            &[
                "-c",
                "printf 'remuda-input-v1\\n'; dd bs=1 count=23 of=/dev/null 2>/dev/null; printf '\\002'; dd bs=1 count=5 of=/dev/null 2>/dev/null; printf '\\001'",
            ],
        )
        .unwrap();
        assert_eq!(
            helper.paste_image_path("/tmp/screenshot.png"),
            Submit::Queued
        );
        assert_eq!(
            helper.recv_event_timeout(Duration::from_secs(3)).unwrap(),
            Event::PasteUnavailable
        );
        assert_eq!(helper.submit(b"x"), Submit::Queued);
        assert_eq!(
            helper.recv_event_timeout(Duration::from_secs(3)).unwrap(),
            Event::Applied
        );
    }

    #[test]
    fn unconsumed_completion_blocks_further_batches() {
        let helper = InputHelper::start(
            Path::new("/bin/sh"),
            &[
                "-c",
                "printf 'remuda-input-v1\\n'; dd bs=1 count=5 of=/dev/null 2>/dev/null; printf '\\001'",
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
        let helper = InputHelper::start(Path::new("/bin/sh"), &["-c", "printf 'remuda-input-v1\\n'; dd bs=1 count=5 of=/dev/null 2>/dev/null; printf '\\001'; dd bs=1 count=5 of=/dev/null 2>/dev/null; printf '\\001'"]).unwrap();
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
        std::process::exit(serve(sender, watch, None, None));
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
