//! Talking to a daemon, including handing your terminal over to one.

use crate::ipc::{self, Stream, TryClone};
use crate::reply_limit::max_reply_wire_bytes;
#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;
use remuda_core::protocol::{Request, Response};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Detach key: Ctrl-\ (0x1C). Chosen because almost nothing binds it, unlike
/// Ctrl-C/D/Z, which the attached program needs. Consumed, never forwarded.
pub const DETACH: u8 = 0x1C;
const ATTACH_INPUT_QUEUE_BYTES: usize = 1024 * 1024;
const ATTACH_INPUT_CHUNK_BYTES: usize = 16 * 1024;
const ATTACH_INPUT_STALL: Duration = Duration::from_secs(3);
const ATTACH_INPUT_DRAIN_GRACE: Duration = Duration::from_millis(500);

fn attach_input_stall_threshold() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(milliseconds) = std::env::var("REMUDA_TEST_ATTACH_STALL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(milliseconds);
    }
    ATTACH_INPUT_STALL
}

#[cfg(debug_assertions)]
fn attach_test_input_hooks_enabled() -> bool {
    std::env::var_os("REMUDA_TEST_INPUT_HOOKS").is_some_and(|value| value == "1")
}

struct AttachInputStatus {
    last_progress: Duration,
    dropping: bool,
    permanent_failure: bool,
    delivered_bytes: u64,
    drop_started_after: u64,
}

struct AttachInputQueue {
    sender: std::sync::mpsc::Sender<Vec<u8>>,
    status: std::sync::Arc<std::sync::Mutex<AttachInputStatus>>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    queued_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    drop_notice_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    recovery_notice_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(debug_assertions)]
    test_hooks_enabled: bool,
    capacity: usize,
    stall_after: Duration,
    now: std::sync::Arc<dyn Fn() -> Duration + Send + Sync>,
}

#[derive(Clone)]
struct AttachInputWriter {
    status: std::sync::Arc<std::sync::Mutex<AttachInputStatus>>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    queued_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    drop_notice_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    recovery_notice_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    now: std::sync::Arc<dyn Fn() -> Duration + Send + Sync>,
}

enum AttachInputAttempt {
    Queued,
    Full(Vec<u8>),
    Dropped,
}

impl AttachInputQueue {
    fn new() -> (Self, std::sync::mpsc::Receiver<Vec<u8>>) {
        Self::with_capacity(ATTACH_INPUT_QUEUE_BYTES, attach_input_stall_threshold())
    }

    fn with_capacity(
        capacity: usize,
        stall_after: Duration,
    ) -> (Self, std::sync::mpsc::Receiver<Vec<u8>>) {
        let started = Instant::now();
        Self::with_clock(
            capacity,
            stall_after,
            std::sync::Arc::new(move || started.elapsed()),
        )
    }

    fn with_clock(
        capacity: usize,
        stall_after: Duration,
        now: std::sync::Arc<dyn Fn() -> Duration + Send + Sync>,
    ) -> (Self, std::sync::mpsc::Receiver<Vec<u8>>) {
        assert!(capacity > 0, "attach input queue capacity must be nonzero");
        let (sender, receiver) = std::sync::mpsc::channel();
        let now_at_start = now();
        (
            Self {
                sender,
                status: std::sync::Arc::new(std::sync::Mutex::new(AttachInputStatus {
                    last_progress: now_at_start,
                    dropping: false,
                    permanent_failure: false,
                    delivered_bytes: 0,
                    drop_started_after: 0,
                })),
                dropped: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                queued_bytes: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                drop_notice_pending: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                recovery_notice_pending: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                    false,
                )),
                #[cfg(debug_assertions)]
                test_hooks_enabled: attach_test_input_hooks_enabled(),
                capacity,
                stall_after,
                now,
            },
            receiver,
        )
    }

    fn attempt(&self, bytes: Vec<u8>) -> AttachInputAttempt {
        let len = bytes.len();
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        if status.dropping {
            if !status.permanent_failure && status.delivered_bytes > status.drop_started_after {
                status.dropping = false;
            } else {
                self.dropped
                    .fetch_add(len, std::sync::atomic::Ordering::SeqCst);
                return AttachInputAttempt::Dropped;
            }
        }
        // Keep the reservation until the writer completes the full chunk, so
        // queued plus in-flight data never exceeds the byte capacity.
        let mut used = self.queued_bytes.load(std::sync::atomic::Ordering::SeqCst);
        loop {
            let Some(next) = used.checked_add(len).filter(|next| *next <= self.capacity) else {
                if (self.now)().saturating_sub(status.last_progress) >= self.stall_after {
                    self.mark_dropping(&mut status);
                    self.dropped
                        .fetch_add(len, std::sync::atomic::Ordering::SeqCst);
                    return AttachInputAttempt::Dropped;
                }
                return AttachInputAttempt::Full(bytes);
            };
            match self.queued_bytes.compare_exchange(
                used,
                next,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => used = actual,
            }
        }
        match self.sender.send(bytes) {
            Ok(()) => AttachInputAttempt::Queued,
            Err(error) => {
                let len = error.0.len();
                self.queued_bytes
                    .fetch_sub(len, std::sync::atomic::Ordering::SeqCst);
                self.mark_dropping(&mut status);
                status.permanent_failure = true;
                self.dropped
                    .fetch_add(len, std::sync::atomic::Ordering::SeqCst);
                AttachInputAttempt::Dropped
            }
        }
    }

    fn writer(&self) -> AttachInputWriter {
        AttachInputWriter {
            status: std::sync::Arc::clone(&self.status),
            dropped: std::sync::Arc::clone(&self.dropped),
            queued_bytes: std::sync::Arc::clone(&self.queued_bytes),
            drop_notice_pending: std::sync::Arc::clone(&self.drop_notice_pending),
            recovery_notice_pending: std::sync::Arc::clone(&self.recovery_notice_pending),
            now: std::sync::Arc::clone(&self.now),
        }
    }

    fn mark_dropping(&self, status: &mut AttachInputStatus) {
        if !status.dropping {
            status.dropping = true;
            status.drop_started_after = status.delivered_bytes;
            self.drop_notice_pending
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn take_drop_notice(&self) -> bool {
        self.drop_notice_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    fn take_recovery_notice(&self) -> bool {
        if self.queued_bytes.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return false;
        }
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if status.dropping || status.permanent_failure {
            return false;
        }
        drop(status);
        self.recovery_notice_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    fn force_paste_close(&self) {
        let len = crate::mouse::PASTE_END.len();
        // This one protocol marker may exceed the byte cap by six bytes. It
        // follows all queued data so the child cannot remain in paste mode.
        self.queued_bytes
            .fetch_add(len, std::sync::atomic::Ordering::SeqCst);
        if self.sender.send(crate::mouse::PASTE_END.to_vec()).is_err() {
            self.queued_bytes
                .fetch_sub(len, std::sync::atomic::Ordering::SeqCst);
            self.dropped
                .fetch_add(len, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn stop_after_drop(&self, bytes: usize) {
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        self.mark_dropping(&mut status);
        self.dropped
            .fetch_add(bytes, std::sync::atomic::Ordering::SeqCst);
    }

    fn enqueue(&self, bytes: &[u8], paste_open_before_chunk: bool, starts_paste: bool) -> bool {
        let mut paste_open_delivered = paste_open_before_chunk && !starts_paste;
        let chunk_size = ATTACH_INPUT_CHUNK_BYTES.min(self.capacity);
        let mut chunks = bytes.chunks(chunk_size).peekable();
        let mut first_chunk = true;
        while let Some(chunk) = chunks.next() {
            let mut pending = chunk.to_vec();
            loop {
                match self.attempt(pending) {
                    AttachInputAttempt::Queued => {
                        if first_chunk && starts_paste {
                            paste_open_delivered = true;
                        }
                        first_chunk = false;
                        break;
                    }
                    AttachInputAttempt::Full(bytes) => {
                        pending = bytes;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    AttachInputAttempt::Dropped => {
                        if paste_open_delivered {
                            self.force_paste_close();
                        }
                        let remaining = chunks.map(<[u8]>::len).sum();
                        self.stop_after_drop(remaining);
                        return true;
                    }
                }
            }
        }
        false
    }
}

impl AttachInputWriter {
    fn progress(&self, count: usize) {
        if count > 0 {
            self.status
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .last_progress = (self.now)();
        }
    }

    fn failed(&self, bytes: usize) {
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        if !status.dropping {
            status.drop_started_after = status.delivered_bytes;
            self.drop_notice_pending
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        status.dropping = true;
        status.permanent_failure = true;
        drop(status);
        self.dropped
            .fetch_add(bytes, std::sync::atomic::Ordering::SeqCst);
        self.queued_bytes
            .fetch_sub(bytes, std::sync::atomic::Ordering::SeqCst);
    }

    fn delivered(&self, bytes: usize) {
        self.queued_bytes
            .fetch_sub(bytes, std::sync::atomic::Ordering::SeqCst);
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        status.delivered_bytes = status.delivered_bytes.saturating_add(bytes as u64);
        if status.dropping
            && !status.permanent_failure
            && status.delivered_bytes > status.drop_started_after
        {
            status.dropping = false;
            self.recovery_notice_pending
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn dropped(&self, bytes: usize) {
        if bytes > 0 {
            self.dropped
                .fetch_add(bytes, std::sync::atomic::Ordering::SeqCst);
            self.queued_bytes
                .fetch_sub(bytes, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn write_attach_chunk(
    stream: &mut Stream,
    bytes: &[u8],
    writer: &AttachInputWriter,
) -> std::io::Result<()> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let written = stream.write(remaining)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "attach input writer made no progress",
            ));
        }
        writer.progress(written);
        remaining = &remaining[written..];
    }
    stream.flush()
}

fn report_attach_input_dropped(output: &mut impl Write, dropped: usize) -> std::io::Result<()> {
    if dropped > 0 {
        writeln!(
            output,
            "\r\n[remuda] input dropped: {dropped} bytes could not be queued or confirmed; delivery may be partial"
        )?;
        output.flush()?;
    }
    Ok(())
}

fn report_attach_drop_started(queue: &AttachInputQueue, output_lock: &std::sync::Mutex<()>) {
    if queue.take_drop_notice() {
        let _guard = output_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut output = std::io::stdout();
        let _ = writeln!(
            output,
            "\r\n[remuda] input delivery stalled; dropping input until the writer recovers"
        );
        let _ = output.flush();
    }
}

fn report_attach_recovered(queue: &AttachInputQueue, output_lock: &std::sync::Mutex<()>) {
    if queue.take_recovery_notice() {
        let _guard = output_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut output = std::io::stdout();
        let _ = writeln!(
            output,
            "\r\n[remuda] input writer recovered; queued input resumed"
        );
        let _ = output.flush();
    }
}

#[cfg(debug_assertions)]
fn report_test_attach_dropped_count(queue: &AttachInputQueue) {
    if queue.test_hooks_enabled {
        if let Some(path) = std::env::var_os("REMUDA_TEST_ATTACH_DROPPED_COUNT") {
            let dropped = queue.dropped.load(std::sync::atomic::Ordering::SeqCst);
            let path = PathBuf::from(path);
            let temporary = path.with_extension("tmp");
            if std::fs::write(&temporary, dropped.to_string()).is_ok() {
                let _ = std::fs::rename(temporary, path);
            }
        }
        if let Some(path) = std::env::var_os("REMUDA_TEST_ATTACH_INPUT_STATE") {
            let queued = queue.queued_bytes.load(std::sync::atomic::Ordering::SeqCst);
            let status = queue.status.lock().unwrap_or_else(|p| p.into_inner());
            let state = format!(
                "queued={queued} delivered={} dropping={}",
                status.delivered_bytes, status.dropping
            );
            drop(status);
            let path = PathBuf::from(path);
            let temporary = path.with_extension("tmp");
            if std::fs::write(&temporary, state).is_ok() {
                let _ = std::fs::rename(temporary, path);
            }
        }
    }
}

#[cfg(not(debug_assertions))]
fn report_test_attach_dropped_count(_queue: &AttachInputQueue) {}

pub const EMPTY_REPLY_ERROR: &str = "the daemon hung up without answering";

const RESET_INPUT_MODES: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l";

fn reset_input_modes(output: &mut impl Write) -> std::io::Result<()> {
    output.write_all(RESET_INPUT_MODES)
}

fn write_input_trace(output: &mut impl Write, at: SystemTime, bytes: &[u8]) -> std::io::Result<()> {
    let elapsed = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    write!(
        output,
        "{}.{:09} ",
        elapsed.as_secs(),
        elapsed.subsec_nanos()
    )?;
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            output.write_all(b" ")?;
        }
        write!(output, "{byte:02x}")?;
    }
    output.write_all(b"\n")
}

fn trace_input_read(path: Option<&Path>, bytes: &[u8]) {
    let Some(path) = path else { return };
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(mut output) = options.open(path) else {
        return;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(metadata) = output.metadata() else {
            return;
        };
        if metadata.permissions().mode() & 0o077 != 0
            && output
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .is_err()
        {
            return;
        }
    }
    let _ = write_input_trace(&mut output, SystemTime::now(), bytes);
}

/// Send one request and read one response.
pub fn request(path: &Path, request: &Request) -> std::io::Result<Response> {
    let stream = ipc::connect(path)?;
    send(&stream, request)?;
    let timeout = match request {
        // Deferred extension replies are capped at 300 seconds by
        // remuda.pending; leave five seconds for delivery and scheduling.
        Request::Eval { .. } => Duration::from_secs(305),
        _ => Duration::from_secs(10),
    };
    read_response_with_timeout(path, stream, timeout)
}

#[cfg(unix)]
fn read_response_with_timeout(
    path: &Path,
    stream: Stream,
    timeout: Duration,
) -> std::io::Result<Response> {
    let deadline = Instant::now() + timeout;
    let limit = max_reply_wire_bytes();
    let mut line = Vec::with_capacity(limit.min(8192));
    let mut bytes = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(request_timeout(path, timeout));
        }
        if let Err(error) = stream.set_recv_timeout(Some(remaining)) {
            // macOS can return EINVAL when SO_RCVTIMEO is set after the peer
            // has already closed. In that case poll until the same deadline;
            // queued reply bytes or EOF make the following read nonblocking.
            if error.kind() != std::io::ErrorKind::InvalidInput {
                return Err(error);
            }
            if !wait_for_socket_readable(&stream, deadline)? {
                return Err(request_timeout(path, timeout));
            }
        }
        match (&stream).read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => {
                let chunk = &bytes[..count];
                let end = chunk.iter().position(|byte| *byte == b'\n');
                let wire_count = end.map_or(count, |end| end + 1);
                if line.len().saturating_add(wire_count) > limit {
                    return Err(reply_too_large());
                }
                line.extend_from_slice(&chunk[..end.unwrap_or(count)]);
                if end.is_some() {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(request_timeout(path, timeout));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let line = String::from_utf8(line)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(interpret(&line))
}

#[cfg(unix)]
fn wait_for_socket_readable(stream: &Stream, deadline: Instant) -> std::io::Result<bool> {
    use std::os::fd::{AsFd, AsRawFd};

    let Stream::UdSocket(socket) = stream;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let milliseconds = remaining
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: socket.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        match unsafe { libc::poll(&mut descriptor, 1, milliseconds) } {
            ready if ready > 0 => return Ok(true),
            0 => {}
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(windows)]
fn read_response_with_timeout(
    path: &Path,
    stream: Stream,
    timeout: Duration,
) -> std::io::Result<Response> {
    // Named pipes do not support nonblocking reads. Keep the same handle in
    // the reader and timeout path so CancelIoEx can release its pending read.
    let stream = std::sync::Arc::new(stream);
    let reader = std::sync::Arc::clone(&stream);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_stop = std::sync::Arc::clone(&stop);
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        if !reader_stop.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = send.send(read_response(&reader));
        }
    });
    match receive.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            ipc::stop_reader(&stream, &stop, || worker.is_finished());
            let _ = worker.join();
            Err(request_timeout(path, timeout))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = worker.join();
            Err(std::io::Error::other(
                "daemon response reader stopped unexpectedly",
            ))
        }
    }
}

fn request_timeout(path: &Path, timeout: Duration) -> std::io::Error {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    #[cfg(unix)]
    let pid = std::fs::read_to_string(&lock_path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u32>().ok());
    #[cfg(windows)]
    let pid: Option<u32> = None;
    let recovery = match pid {
        Some(pid) => format!(
            "daemon pid {pid}; if it is stopped, run `kill -CONT {pid}` to resume it; if stuck, verify it is this daemon, run `kill {pid}`, then retry"
        ),
        None => format!(
            "daemon pid unavailable (inspect {}); if it is stopped, resume it; if stuck, verify it is this daemon before killing it, then retry",
            lock_path.display()
        ),
    };
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "timed out after {}s waiting for daemon response on socket {} ({recovery})",
            timeout.as_secs(),
            path.display()
        ),
    )
}

/// Send one request with a bounded wait for its local daemon reply. Waking the
/// cloned stream interrupts the worker's pending read on Unix and Windows.
pub fn request_with_timeout(
    path: &Path,
    request: &Request,
    timeout: std::time::Duration,
) -> std::io::Result<Response> {
    let stream = ipc::connect(path)?;
    let wake_stream = stream.try_clone()?;
    let request = request.clone();
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = send(&stream, &request).and_then(|()| read_response(&stream));
        let _ = reply_tx.send(result);
    });
    match reply_rx.recv_timeout(timeout) {
        Ok(result) => {
            worker
                .join()
                .map_err(|_| std::io::Error::other("daemon request worker panicked"))?;
            result
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            ipc::wake(&wake_stream);
            let _ = worker.join();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "daemon request timed out",
            ))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = worker.join();
            Err(std::io::Error::other("daemon request worker stopped"))
        }
    }
}

fn send(mut stream: &Stream, request: &Request) -> std::io::Result<()> {
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

fn read_response(stream: &Stream) -> std::io::Result<Response> {
    let limit = max_reply_wire_bytes();
    let mut reader = BufReader::new(stream.take(limit.saturating_add(1) as u64));
    let mut bytes = Vec::with_capacity(8192);
    reader.read_until(b'\n', &mut bytes)?;
    if bytes.len() > limit {
        return Err(reply_too_large());
    }
    let line = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(interpret(&line))
}

fn reply_too_large() -> std::io::Error {
    let limit = max_reply_wire_bytes();
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "daemon reply exceeds the {} MiB wire limit",
            limit / (1024 * 1024)
        ),
    )
}

/// Read one bounded protocol line without buffering bytes beyond its newline.
fn read_bounded_line(stream: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let limit = max_reply_wire_bytes();
    let mut line = Vec::with_capacity(limit.min(8192));
    let mut byte = [0u8; 1];
    while line.len() <= limit {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    if line.len() > limit {
        return Err(reply_too_large());
    }
    Ok(line)
}

/// What a daemon says when it cannot READ what we sent. Only the deserializer
/// produces it — every refusal about content ("no such session") is worded by a
/// handler — so this prefix means one thing and nothing else.
const CANNOT_READ: &str = "bad request: ";

/// One reply, with the two shapes of version skew turned into words a person
/// can act on. Everything else is passed through exactly as the daemon wrote it:
/// a malformed request that is NOT skew must still say what it was.
fn interpret(line: &str) -> Response {
    if line.trim().is_empty() {
        return Response::error(EMPTY_REPLY_ERROR);
    }
    match serde_json::from_str::<Response>(line) {
        // We serialized that request from this binary's own `Request`, so a
        // daemon on this build CANNOT fail to read it. One that did is not.
        Ok(Response::Error(reason)) => match reason.strip_prefix(CANNOT_READ) {
            Some(detail) => {
                Response::error(skew(&format!("it could not read the request: {detail}")))
            }
            None => Response::Error(reason),
        },
        Ok(other) => other,
        // The mirror: a reply we cannot read, from a daemon newer than we are.
        Err(e) => Response::error(skew(&format!("its reply did not parse: {e}"))),
    }
}

/// Read one protocol line without buffering beyond it. The attach client keeps
/// this exact Windows pipe handle for the output pump so its detach wake can
/// cancel the pending read on the same handle.
fn read_protocol_line(stream: &mut impl Read) -> std::io::Result<String> {
    let mut line = read_bounded_line(stream)?;
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    String::from_utf8(line)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Every daemon deployed before any handshake existed can never announce its own
/// version, so the client's reaction to the failure is the only diagnosis that
/// reaches those users. See [`tests::the_cure_survives_an_eighty_column_crop`].
fn skew(detail: &str) -> String {
    // One line, cure first. The TUI footer is one row of `cols` and crops the
    // tail: measured at 80 columns, a message that led with the diagnosis lost
    // `remuda stop` off the right edge — the only actionable half of it.
    format!(
        "the daemon is not this build — run `remuda stop`, then this again. \
         This command is {}; the daemon: {detail}",
        crate::dist::BUILD_VERSION,
    )
}

/// Which way a ride ended. Both used to return `Ok(())`, and the terminal
/// looked identical either way — that is the incident this exists for: a second
/// `exit` went to the user's real login shell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Left {
    Detached,
    Exited,
    TakenOver,
}

/// Give this terminal to a session until the user presses [`DETACH`] or the
/// session ends. Leaving does not disturb the session: the process keeps
/// running and this direct attach never changes its size.
pub fn attach(path: &Path, name: &str) -> std::io::Result<Left> {
    attach_with_mouse(path, name, true)
}

enum AttachAck {
    Tracked(u64),
    Legacy,
    Unsupported,
    Refused(String),
}

#[derive(Default)]
struct AttachScrollback {
    offset: std::sync::atomic::AtomicUsize,
    history_total: std::sync::atomic::AtomicUsize,
}

fn attach_ack(line: &str) -> AttachAck {
    match interpret(line) {
        Response::AttachStarted { generation } => AttachAck::Tracked(generation),
        Response::Ok => AttachAck::Legacy,
        Response::Error(reason) if reason.starts_with("the daemon is not this build") => {
            AttachAck::Unsupported
        }
        Response::Error(reason) => AttachAck::Refused(reason),
        _ => AttachAck::Refused("daemon did not acknowledge attach".into()),
    }
}

fn begin_attach(path: &Path, name: &str) -> std::io::Result<(Stream, Option<u64>)> {
    let mut stream = ipc::connect(path)?;
    send(&stream, &Request::AttachTracked { name: name.into() })?;
    match attach_ack(&read_protocol_line(&mut stream)?) {
        AttachAck::Tracked(generation) => Ok((stream, Some(generation))),
        AttachAck::Legacy => Ok((stream, None)),
        AttachAck::Unsupported => {
            drop(stream);
            let mut stream = ipc::connect(path)?;
            send(&stream, &Request::Attach { name: name.into() })?;
            match interpret(&read_protocol_line(&mut stream)?) {
                Response::Ok => Ok((stream, None)),
                Response::Error(reason) => Err(std::io::Error::other(reason)),
                _ => Err(std::io::Error::other("daemon did not acknowledge attach")),
            }
        }
        AttachAck::Refused(reason) => Err(std::io::Error::other(reason)),
    }
}

fn was_taken_over(path: &Path, name: &str, generation: Option<u64>) -> bool {
    generation.is_some_and(|generation| {
        matches!(
            request(
                path,
                &Request::AttachStatus {
                    name: name.into(),
                    generation
                }
            ),
            Ok(Response::AttachStatus { taken_over: true })
        )
    })
}

// This lifecycle is intentionally linear: attach, start the input reader,
// drain output, wake the reader and restore the terminal in one scope.
#[allow(clippy::too_many_lines)]
pub fn attach_with_mouse(path: &Path, name: &str, mouse: bool) -> std::io::Result<Left> {
    let (stream, generation) = begin_attach(path, name)?;

    let _raw = RawMode::enable()?;
    if mouse {
        let mut stdout = std::io::stdout();
        stdout.write_all(b"\x1b[?1000h\x1b[?1006h")?;
        stdout.flush()?;
    }

    // The Windows wake must cancel the same HANDLE that owns the blocked read;
    // cloning a named-pipe stream creates a different HANDLE. Share this one
    // between the output pump and the key thread that may need to wake it.
    let reader_stream = std::sync::Arc::new(stream);

    // Only the key thread can tell the two exits apart: the reader below just
    // sees the stream end, which is true of a detach and of a death alike.
    let detached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_taken_over = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let trace_input = std::env::var_os("REMUDA_TRACE_INPUT").map(PathBuf::from);
    let trace_attach_exit = std::env::var_os("REMUDA_TRACE_ATTACH_EXIT").is_some();
    let scrollback = std::sync::Arc::new(AttachScrollback::default());
    let output_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    let (input_queue, input_rx) = AttachInputQueue::new();
    let input_dropped = std::sync::Arc::clone(&input_queue.dropped);
    let input_writer_status = input_queue.writer();
    let input_writer_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_stream =
        std::sync::Arc::new(std::sync::Mutex::new(reader_stream.as_ref().try_clone()?));
    let writer_wake = ipc::wake_handle(
        &writer_stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    let (writer_done_tx, writer_done_rx) = std::sync::mpsc::channel();
    let input_writer = {
        let writer_stream = std::sync::Arc::clone(&writer_stream);
        let writer_status = input_writer_status;
        let stop = std::sync::Arc::clone(&input_writer_stop);
        std::thread::spawn(move || {
            loop {
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let bytes = match input_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(bytes) => bytes,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let mut stream = writer_stream
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if write_attach_chunk(&mut stream, &bytes, &writer_status).is_err() {
                    writer_status.failed(bytes.len());
                    break;
                }
                writer_status.delivered(bytes.len());
            }
            let mut queued_after_stop = 0;
            while let Ok(bytes) = input_rx.try_recv() {
                queued_after_stop += bytes.len();
            }
            writer_status.dropped(queued_after_stop);
            let _ = writer_done_tx.send(());
        })
    };
    let attach_path = path.to_path_buf();
    let attach_name = name.to_string();

    // Keystrokes out, on their own thread; the screen pump runs here.
    let keys = std::thread::spawn({
        let reader_stream = std::sync::Arc::clone(&reader_stream);
        let detached = std::sync::Arc::clone(&detached);
        let output_taken_over = std::sync::Arc::clone(&output_taken_over);
        let output_stop = std::sync::Arc::clone(&output_stop);
        let output_done = std::sync::Arc::clone(&output_done);
        let scrollback = std::sync::Arc::clone(&scrollback);
        let output_lock = std::sync::Arc::clone(&output_lock);
        let input_writer_stop = std::sync::Arc::clone(&input_writer_stop);
        let writer_done_rx = writer_done_rx;
        let writer_stream = std::sync::Arc::clone(&writer_stream);
        let path = attach_path.clone();
        let name = attach_name.clone();
        move || {
            let _writer_stream_lifetime = writer_stream;
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 1024];
            let mut parser = crate::mouse::SgrParser::default();
            let mut mouse_on = mouse;
            let mut route = AttachRoute {
                path: &path,
                name: &name,
                input: &input_queue,
                mouse_on: &mut mouse_on,
                mouse_toggle_enabled: mouse,
                scrollback: &scrollback,
                output_lock: &output_lock,
                discarding_paste: false,
            };
            let mut logged_pending_read = false;
            loop {
                report_attach_drop_started(&input_queue, &output_lock);
                report_attach_recovered(&input_queue, &output_lock);
                report_test_attach_dropped_count(&input_queue);
                if output_done.load(std::sync::atomic::Ordering::SeqCst) {
                    if output_taken_over.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    // Keep the advertised "press any key" behavior after the
                    // child exits, but let that key release the attach and
                    // restore terminal modes instead of routing it to nowhere.
                    #[cfg(unix)]
                    {
                        let post_exit_read = read_stdin_timeout(
                            &mut stdin,
                            &mut buf,
                            std::time::Duration::from_secs(86_400),
                        );
                        match post_exit_read {
                            Ok(Some(_)) => {
                                if trace_attach_exit {
                                    eprintln!("attach input trace: post-exit forwarder read a key");
                                }
                                break;
                            }
                            Ok(None) => continue,
                            Err(error) => {
                                if trace_attach_exit {
                                    eprintln!("attach input trace: post-exit read error: {error}");
                                }
                                break;
                            }
                        }
                    }
                    #[cfg(windows)]
                    {
                        if trace_attach_exit {
                            eprintln!("attach input trace: post-exit ReadConsoleInputW entered");
                        }
                        // Once output has ended, wait directly on the console
                        // input queue. ConPTY keys do not reliably wake the
                        // handle through WaitForSingleObject.
                        let result = wait_for_windows_keypress();
                        if trace_attach_exit {
                            eprintln!("attach input trace: post-exit ReadConsoleInputW returned: {result:?}");
                        }
                        break;
                    }
                }
                let wait = parser
                    .timeout_remaining()
                    .unwrap_or(std::time::Duration::from_millis(25))
                    .min(std::time::Duration::from_millis(25));
                if trace_attach_exit && !logged_pending_read {
                    eprintln!(
                        "attach input trace: forwarder entering stdin read; output_done={}",
                        output_done.load(std::sync::atomic::Ordering::SeqCst)
                    );
                    logged_pending_read = true;
                }
                let read = read_stdin_timeout(&mut stdin, &mut buf, wait);
                if trace_attach_exit {
                    match &read {
                        Ok(Some(n)) => {
                            eprintln!(
                                "attach input trace: forwarder read {n} bytes; output_done={}",
                                output_done.load(std::sync::atomic::Ordering::SeqCst)
                            );
                            logged_pending_read = false;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            eprintln!("attach input trace: forwarder read error: {error}");
                            logged_pending_read = false;
                        }
                    }
                }
                let n = match read {
                    Ok(Some(n)) => n,
                    Ok(None) => {
                        route_tokens(&mut route, parser.flush_expired());
                        continue;
                    }
                    Err(_) => break,
                };
                if output_done.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                if n == 0 {
                    break;
                }
                trace_input_read(trace_input.as_deref(), &buf[..n]);
                let detach_at = detach_offset(&parser, &buf[..n]);
                match detach_at {
                    // Forward what was typed before the detach key, then stop.
                    // Dropping those bytes would silently swallow input the
                    // user believes they sent.
                    Some(at) => {
                        route_tokens(&mut route, parser.feed(&buf[..at]));
                        route_tokens(&mut route, parser.finish());
                        detached.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    None => {
                        route_tokens(&mut route, parser.feed(&buf[..n]));
                    }
                }
            }
            // stdin EOF/errors and child exit can end this thread mid-paste.
            // Always restore the child parser before dropping its input pipe.
            if parser.paste_open() {
                route_tokens(&mut route, parser.finish());
            }
            // Ends the screen pump below, which then returns from `attach` and
            // drops every handle on this connection — that hang-up is what the
            // daemon reads as "the human left".
            report_attach_drop_started(&input_queue, &output_lock);
            drop(input_queue);
            if writer_done_rx
                .recv_timeout(ATTACH_INPUT_DRAIN_GRACE)
                .is_err()
            {
                input_writer_stop.store(true, std::sync::atomic::Ordering::SeqCst);
                ipc::wake_captured(writer_wake);
                let _ = writer_done_rx.recv_timeout(Duration::from_secs(1));
            }
            ipc::stop_reader(&reader_stream, &output_stop, || {
                output_done.load(std::sync::atomic::Ordering::SeqCst)
            });
        }
    });

    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 8192];
    let mut reader = reader_stream.as_ref();
    let output_end = loop {
        if output_stop.load(std::sync::atomic::Ordering::SeqCst) {
            break "stopped".to_string();
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break "EOF".to_string(),
            Ok(n) => n,
            Err(error) => break format!("read error: {error}"),
        };
        let _guard = output_lock.lock().unwrap_or_else(|e| e.into_inner());
        if scrollback.offset.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            sync_scrollback_anchor(path, name, &scrollback);
            continue;
        }
        if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
            break "stdout write failed".to_string();
        }
    };
    if trace_attach_exit {
        eprintln!("attach input trace: session output reader ended: {output_end}");
    }

    let taken_over = was_taken_over(path, name, generation);
    output_taken_over.store(taken_over, std::sync::atomic::Ordering::SeqCst);
    output_done.store(true, std::sync::atomic::Ordering::SeqCst);
    ipc::stop_reader(&reader_stream, &output_stop, || {
        output_done.load(std::sync::atomic::Ordering::SeqCst)
    });
    let left = if taken_over {
        Left::TakenOver
    } else if detached.load(std::sync::atomic::Ordering::SeqCst) {
        Left::Detached
    } else {
        // The key thread remains alive until one key releases the user's
        // terminal after the session exits.
        let _ = write!(stdout, "\r\n[remuda] {name} ended — press any key\r\n");
        let _ = stdout.flush();
        Left::Exited
    };
    #[cfg(windows)]
    if taken_over {
        // ConPTY stdin may be blocked in a synchronous console read. Once the
        // takeover outcome is known, returning lets the CLI exit and Windows
        // tear down that worker instead of waiting forever in `join`.
        drop(keys);
    } else {
        let _ = keys.join();
    }
    #[cfg(unix)]
    let _ = keys.join();
    input_writer_stop.store(true, std::sync::atomic::Ordering::SeqCst);
    ipc::wake_captured(writer_wake);
    let _ = input_writer.join();
    let dropped = input_dropped.load(std::sync::atomic::Ordering::SeqCst);
    let _ = report_attach_input_dropped(&mut stdout, dropped);
    Ok(left)
}

#[cfg(windows)]
fn wait_for_windows_keypress() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
    };

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut records = [INPUT_RECORD::default(); 16];
    loop {
        let mut count = 0;
        if unsafe {
            ReadConsoleInputW(
                handle,
                records.as_mut_ptr(),
                records.len() as u32,
                &mut count,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        for record in records.iter().take(count as usize) {
            if u32::from(record.EventType) == KEY_EVENT {
                // Console input queues include key-up records as well. Only a
                // key-down should release the post-exit wait.
                let key = unsafe { record.Event.KeyEvent };
                if key.bKeyDown != 0 && key.wVirtualKeyCode != 0 {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(unix)]
fn read_stdin_timeout(
    _stdin: &mut impl Read,
    buf: &mut [u8],
    timeout: std::time::Duration,
) -> std::io::Result<Option<usize>> {
    use std::os::fd::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, millis) };
        if ready > 0 {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            return if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(Some(n as usize))
            };
        }
        if ready == 0 {
            return Ok(None);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[cfg(windows)]
fn read_stdin_timeout(
    stdin: &mut impl Read,
    buf: &mut [u8],
    timeout: std::time::Duration,
) -> std::io::Result<Option<usize>> {
    use windows_sys::Win32::{
        Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Console::{GetStdHandle, STD_INPUT_HANDLE},
            Threading::WaitForSingleObject,
        },
    };
    let handle: HANDLE = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    match unsafe { WaitForSingleObject(handle, millis) } {
        WAIT_OBJECT_0 => stdin.read(buf).map(Some),
        WAIT_TIMEOUT => Ok(None),
        _ => Err(std::io::Error::last_os_error()),
    }
}

struct AttachRoute<'a> {
    path: &'a Path,
    name: &'a str,
    input: &'a AttachInputQueue,
    mouse_on: &'a mut bool,
    mouse_toggle_enabled: bool,
    scrollback: &'a AttachScrollback,
    output_lock: &'a std::sync::Mutex<()>,
    discarding_paste: bool,
}

fn route_tokens(route: &mut AttachRoute<'_>, tokens: Vec<crate::mouse::InputToken>) {
    use crate::mouse::{route_mouse_event, InputToken, MouseAction};
    use remuda_core::agent::MouseState;
    use std::sync::atomic::Ordering;

    for token in tokens {
        match token {
            InputToken::Mouse(event) => {
                let state = if *route.mouse_on {
                    match request(
                        route.path,
                        &Request::MouseState {
                            name: route.name.into(),
                        },
                    ) {
                        Ok(Response::MouseState(state)) => state,
                        _ => MouseState::default(),
                    }
                } else {
                    MouseState::default()
                };
                if *route.mouse_on
                    && state.mode == remuda_core::agent::MouseMode::None
                    && route_scrollback_wheel(
                        route.path,
                        route.name,
                        event,
                        route.scrollback,
                        route.output_lock,
                    )
                {
                    continue;
                }
                let current = route.scrollback.offset.load(Ordering::SeqCst);
                match route_mouse_event(event, state, *route.mouse_on, current) {
                    MouseAction::Forward(bytes) => {
                        let _ = route.input.enqueue(&bytes, false, false);
                    }
                    MouseAction::Scroll(next) => {
                        let _guard = route.output_lock.lock().unwrap_or_else(|e| e.into_inner());
                        route.scrollback.offset.store(next, Ordering::SeqCst);
                        if let Some((_, total)) = paint_history(route.path, route.name, next) {
                            route
                                .scrollback
                                .history_total
                                .store(total, Ordering::SeqCst);
                        }
                    }
                    MouseAction::Ignore => {}
                }
            }
            InputToken::Bytes(bytes) => {
                if route_scrollback_page(
                    route.path,
                    route.name,
                    &bytes,
                    *route.mouse_on,
                    route.scrollback,
                    route.output_lock,
                ) {
                    continue;
                }
                let mut start = 0;
                for (at, &byte) in bytes.iter().enumerate() {
                    if byte == 0x1d && route.mouse_toggle_enabled {
                        if start < at {
                            exit_history_if_needed(route, &bytes[start..at]);
                        }
                        *route.mouse_on = !*route.mouse_on;
                        let mut stdout = std::io::stdout();
                        let _guard = route.output_lock.lock().unwrap_or_else(|e| e.into_inner());
                        let report = if *route.mouse_on {
                            b"\x1b[?1000h\x1b[?1006h"
                        } else {
                            b"\x1b[?1000l\x1b[?1006l"
                        };
                        let _ = stdout.write_all(report);
                        let _ = stdout.flush();
                        start = at + 1;
                    }
                }
                if start < bytes.len() {
                    exit_history_if_needed(route, &bytes[start..]);
                }
            }
            InputToken::Paste(bytes) => {
                if route.discarding_paste {
                    if bytes.ends {
                        route.discarding_paste = false;
                    }
                    continue;
                }
                let _ = exit_history_if_needed(route, &[]);
                let dropped = route
                    .input
                    .enqueue(&bytes.bytes, !bytes.starts, bytes.starts);
                route.discarding_paste = dropped && !bytes.ends;
            }
        }
    }
}

/// Handle PageUp/PageDown locally only when the child has not enabled mouse
/// reporting. At live offset zero, leave the key for the child.
fn route_scrollback_page(
    path: &Path,
    name: &str,
    bytes: &[u8],
    mouse_on: bool,
    scrollback: &AttachScrollback,
    output_lock: &std::sync::Mutex<()>,
) -> bool {
    use std::sync::atomic::Ordering;

    if !mouse_on || (bytes != b"\x1b[5~" && bytes != b"\x1b[6~") {
        return false;
    }
    let child_tracks_mouse = matches!(
        request(path, &Request::MouseState { name: name.into() }),
        Ok(Response::MouseState(state)) if state.mode != remuda_core::agent::MouseMode::None
    );
    if child_tracks_mouse {
        return false;
    }

    let _guard = output_lock.lock().unwrap_or_else(|e| e.into_inner());
    sync_scrollback_anchor(path, name, scrollback);
    let old = scrollback.offset.load(Ordering::SeqCst);
    let history_rows = history_metadata(path, name).map_or(0, |(rows, _)| rows);
    let next = if bytes == b"\x1b[5~" {
        old.saturating_add(24).min(history_rows)
    } else {
        old.saturating_sub(24)
    };
    if next != old {
        scrollback.offset.store(next, Ordering::SeqCst);
        if let Some((_, total)) = paint_history(path, name, next) {
            scrollback.history_total.store(total, Ordering::SeqCst);
        }
        return true;
    }
    old != 0
}

fn route_scrollback_wheel(
    path: &Path,
    name: &str,
    event: crate::mouse::SgrMouse,
    scrollback: &AttachScrollback,
    output_lock: &std::sync::Mutex<()>,
) -> bool {
    use crate::mouse::{scroll_offset, wheel_delta};
    use std::sync::atomic::Ordering;

    let Some(delta) = wheel_delta(event) else {
        return false;
    };
    let _guard = output_lock.lock().unwrap_or_else(|e| e.into_inner());
    sync_scrollback_anchor(path, name, scrollback);
    let current = scrollback.offset.load(Ordering::SeqCst);
    let Some((history_rows, _)) = history_metadata(path, name) else {
        return true;
    };
    let next = scroll_offset(current, delta, history_rows);
    if next != current {
        scrollback.offset.store(next, Ordering::SeqCst);
        if let Some((_, total)) = paint_history(path, name, next) {
            scrollback.history_total.store(total, Ordering::SeqCst);
        }
    }
    true
}

fn exit_history_if_needed(route: &mut AttachRoute<'_>, bytes: &[u8]) -> bool {
    use std::sync::atomic::Ordering;
    let _guard = route.output_lock.lock().unwrap_or_else(|e| e.into_inner());
    let old = route.scrollback.offset.load(Ordering::SeqCst);
    if old != 0 {
        if bytes == b"q" || bytes == b"\x1b" || bytes == b"\x1bq" {
            route.scrollback.offset.store(0, Ordering::SeqCst);
            if let Some((_, total)) = paint_history(route.path, route.name, 0) {
                route
                    .scrollback
                    .history_total
                    .store(total, Ordering::SeqCst);
            }
            return false;
        }
        route.scrollback.offset.store(0, Ordering::SeqCst);
        if let Some((_, total)) = paint_history(route.path, route.name, 0) {
            route
                .scrollback
                .history_total
                .store(total, Ordering::SeqCst);
        }
    }
    drop(_guard);
    let dropped = route.input.enqueue(bytes, false, false);
    #[cfg(not(debug_assertions))]
    let _ = dropped;
    #[cfg(debug_assertions)]
    if route.input.test_hooks_enabled
        && bytes
            .windows(b"DRAINED-BARRIER".len())
            .any(|window| window == b"DRAINED-BARRIER")
    {
        if let Some(path) = std::env::var_os("REMUDA_TEST_ATTACH_BARRIER_RESULT") {
            let result: &[u8] = if dropped { b"dropped" } else { b"queued" };
            let _ = std::fs::write(path, result);
        }
    }
    dropped
}

fn detach_offset(parser: &crate::mouse::SgrParser, bytes: &[u8]) -> Option<usize> {
    if parser.paste_open()
        && bytes == [DETACH]
        && parser.paste_idle_at_least(std::time::Duration::from_secs(1))
    {
        // While a bracketed paste is live, Ctrl-\\ is data unless it arrives
        // alone after the paste has gone idle.
        Some(0)
    } else {
        parser.first_byte_outside_paste(bytes, DETACH)
    }
}

/// Paint one captured frame while the caller holds the output lock.
fn paint_history(path: &Path, name: &str, offset: usize) -> Option<(usize, usize)> {
    use remuda_core::agent::Color;
    let Ok(Response::StyledScreen {
        rows,
        cursor,
        scrollback_len,
        scrollback_total,
        ..
    }) = request(
        path,
        &Request::CaptureStyled {
            name: name.into(),
            scrollback: offset,
        },
    )
    else {
        return None;
    };
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[H\x1b[2J");
    for (index, row) in rows.iter().enumerate() {
        for run in row {
            let mut codes = vec![
                if run.bold { "1" } else { "22" }.to_string(),
                if run.dim { "2" } else { "22" }.to_string(),
                if run.italic { "3" } else { "23" }.to_string(),
                if run.underline { "4" } else { "24" }.to_string(),
                if run.inverse { "7" } else { "27" }.to_string(),
            ];
            let color_codes = |color: Color, foreground: bool| match color {
                Color::Default => vec![if foreground { "39" } else { "49" }.to_string()],
                Color::Idx(index) => vec![
                    if foreground { "38" } else { "48" }.to_string(),
                    "5".into(),
                    index.to_string(),
                ],
                Color::Rgb(r, g, b) => vec![
                    if foreground { "38" } else { "48" }.to_string(),
                    "2".into(),
                    r.to_string(),
                    g.to_string(),
                    b.to_string(),
                ],
            };
            codes.extend(color_codes(run.fg, true));
            codes.extend(color_codes(run.bg, false));
            let _ = write!(stdout, "\x1b[{}m", codes.join(";"));
            let _ = stdout.write_all(run.text.as_bytes());
            let _ = stdout.write_all(b"\x1b[0m");
        }
        if index + 1 < rows.len() {
            let _ = stdout.write_all(b"\r\n");
        }
    }
    if offset != 0 {
        let columns = crossterm::terminal::size().map_or(80, |(columns, _)| usize::from(columns));
        let indicator = truncate_terminal_text(
            &format!("[scrollback: {offset} rows · q/Esc exit · keys go live]"),
            columns,
        );
        let _ = write!(stdout, "\x1b[{};1H\x1b[2K{indicator}", rows.len().max(1),);
    }
    if offset == 0 {
        let _ = if cursor.visible {
            write!(
                stdout,
                "\x1b[{};{}H\x1b[?25h",
                cursor.row + 1,
                cursor.col + 1
            )
        } else {
            stdout.write_all(b"\x1b[?25l")
        };
    } else {
        let _ = stdout.write_all(b"\x1b[?25l");
    }
    let _ = stdout.flush();
    Some((scrollback_len, scrollback_total))
}

fn truncate_terminal_text(text: &str, max_columns: usize) -> String {
    use unicode_width::UnicodeWidthChar;

    let mut result = String::new();
    let mut columns = 0;
    for character in text.chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width > max_columns.saturating_sub(columns) {
            break;
        }
        result.push(character);
        columns += width;
    }
    result
}

fn history_metadata(path: &Path, name: &str) -> Option<(usize, usize)> {
    match request(
        path,
        &Request::CaptureStyled {
            name: name.into(),
            scrollback: 0,
        },
    ) {
        Ok(Response::StyledScreen {
            scrollback_len,
            scrollback_total,
            ..
        }) => Some((scrollback_len, scrollback_total)),
        _ => None,
    }
}

/// Advance a paused direct-attach view with output while keeping its absolute
/// history rows in view. The caller holds the output lock.
fn sync_scrollback_anchor(path: &Path, name: &str, scrollback: &AttachScrollback) {
    use std::sync::atomic::Ordering;

    let offset = scrollback.offset.load(Ordering::SeqCst);
    if offset == 0 {
        return;
    }
    let previous_total = scrollback.history_total.load(Ordering::SeqCst);
    let Some((history_rows, current_total)) = history_metadata(path, name) else {
        return;
    };
    let next = crate::mouse::anchor_offset_to_new_history(
        offset,
        previous_total,
        current_total,
        history_rows,
    );
    scrollback.offset.store(next, Ordering::SeqCst);
    scrollback
        .history_total
        .store(current_total, Ordering::SeqCst);
    if current_total == previous_total && next == offset {
        return;
    }

    if let Some((paint_rows, painted_total)) = paint_history(path, name, next) {
        let anchored = crate::mouse::anchor_offset_to_new_history(
            next,
            current_total,
            painted_total,
            paint_rows,
        );
        if anchored != next {
            scrollback.offset.store(anchored, Ordering::SeqCst);
            if let Some((_, final_total)) = paint_history(path, name, anchored) {
                scrollback
                    .history_total
                    .store(final_total, Ordering::SeqCst);
            }
        } else {
            scrollback
                .history_total
                .store(painted_total, Ordering::SeqCst);
        }
    }
}

/// A session held for a human typing into a pane rather than into the whole
/// terminal. The same exclusive `Attach` a ride takes, so orchestrated input is
/// refused while a person drives — PRINCIPLES §6, invariant 3.
pub struct Hold {
    stream: Stream,
    drain: Option<std::thread::JoinHandle<()>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Take a session for the TUI's focused pane. The output is *not* handed back:
/// a pane repaints from `Capture`, which crops to its own width and cannot be
/// fed raw pty bytes aimed at a whole terminal.
pub fn hold(path: &Path, name: &str) -> std::io::Result<Hold> {
    hold_inner(path, name, std::time::Duration::ZERO)
}

/// [TEST-ONLY] Like `hold`, but the drain thread sleeps `drain_delay` before
/// its first read, forcing the Windows same-thread-drop race regardless of
/// caller timing. See steps/029.
pub fn hold_with_drain_delay(
    path: &Path,
    name: &str,
    drain_delay: std::time::Duration,
) -> std::io::Result<Hold> {
    hold_inner(path, name, drain_delay)
}

fn hold_inner(path: &Path, name: &str, drain_delay: std::time::Duration) -> std::io::Result<Hold> {
    let stream = ipc::connect(path)?;
    send(
        &stream,
        &Request::Attach {
            name: name.to_string(),
        },
    )?;
    let mut reader = stream.try_clone()?;
    let line = String::from_utf8(read_bounded_line(&mut reader)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    match interpret(&line) {
        Response::Ok => {}
        Response::Error(reason) => return Err(std::io::Error::other(reason)),
        _ => return Err(std::io::Error::other("daemon did not acknowledge attach")),
    }
    // Drained rather than ignored: the daemon repaints and then streams, and an
    // unread socket fills and parks its output pump.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drain_stop = stop.clone();
    let drain = std::thread::spawn(move || {
        if !drain_delay.is_zero() {
            std::thread::sleep(drain_delay);
        }
        let mut buf = [0u8; 8192];
        while !drain_stop.load(std::sync::atomic::Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    Ok(Hold {
        stream,
        drain: Some(drain),
        stop,
    })
}

impl Hold {
    /// Type exactly these bytes: one `write_all`, nothing appended — the terms
    /// `Attached::write_raw` sets one layer down.
    pub fn keys(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut stream = &self.stream;
        stream.write_all(bytes)?;
        stream.flush()
    }
}

impl Drop for Hold {
    /// Hanging up is what releases the guard, so it has to survive a panic or an
    /// early return — a guard outliving its viewer is the deadlock step 012 met.
    fn drop(&mut self) {
        if let Some(drain) = self.drain.take() {
            ipc::stop_reader(&self.stream, &self.stop, || drain.is_finished());
            let _ = drain.join();
        }
    }
}

/// Raw mode plus the alternate screen, restored on the way out. Restoring must
/// stay in `Drop`: the exits that matter — an error, a panic, a `?` — are
/// exactly the ones that skip the end of `attach`.
pub struct RawMode;

impl RawMode {
    /// The alternate screen is what makes leaving *visible*: your scrollback
    /// and prompt come back, so a second `exit` cannot be aimed at the wrong
    /// shell by a screen that never changed.
    pub fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        if let Err(e) =
            crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)
        {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(e);
        }
        Ok(Self)
    }
}

impl Drop for RawMode {
    /// Leave the alternate screen *before* termios goes back, so the last thing
    /// the terminal does in raw mode is the buffer switch.
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = reset_input_modes(&mut stdout);
        let _ = crossterm::execute!(
            stdout,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        // Keep the protocol reset and alternate-screen exit ordered on the
        // terminal before process shutdown, including on Windows where stdout
        // may still have buffered writes at this point.
        let _ = stdout.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::trace_input_read;
    use super::{
        detach_offset, interpret, report_attach_input_dropped, reset_input_modes, route_tokens,
        truncate_terminal_text, write_input_trace, AttachInputQueue, AttachRoute,
        ATTACH_INPUT_STALL, DETACH, RESET_INPUT_MODES,
    };
    #[cfg(unix)]
    use super::{read_response_with_timeout, request_with_timeout};
    #[cfg(unix)]
    use crate::ipc;
    #[cfg(unix)]
    use interprocess::local_socket::traits::Listener as _;
    #[cfg(unix)]
    use remuda_core::protocol::Request;
    use remuda_core::protocol::Response;
    use std::path::Path;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicU64, Ordering};
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::time::Instant;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn attach_input_drop_recovers_after_writer_progress() {
        let (queue, rx) = AttachInputQueue::with_capacity(6, Duration::ZERO);
        queue.enqueue(b"queued", false, false);
        queue.enqueue(b"dropped", false, false);
        assert!(queue.take_drop_notice());
        assert!(!queue.take_drop_notice());
        assert_eq!(queue.dropped.load(std::sync::atomic::Ordering::SeqCst), 7);
        queue.writer().delivered(6);
        queue.enqueue(b"fresh!", false, false);
        assert_eq!(rx.try_recv().unwrap(), b"queued");
        assert_eq!(rx.try_recv().unwrap(), b"fresh!");
        assert!(rx.try_recv().is_err());
        let mut notice = Vec::new();
        report_attach_input_dropped(&mut notice, 7).unwrap();
        let notice = String::from_utf8(notice).unwrap();
        assert!(notice.contains("input dropped: 7 bytes"));
        assert!(notice.contains("delivery may be partial"));
    }

    #[test]
    fn one_notice_per_drop_until_a_chunk_is_delivered() {
        let (queue, rx) = AttachInputQueue::with_capacity(6, Duration::ZERO);
        queue.enqueue(b"abcd", false, false);
        queue.enqueue(b"xyz", false, false);
        assert!(queue.take_drop_notice());
        queue.enqueue(b"xyz", false, false);
        assert!(!queue.take_drop_notice());

        queue.writer().delivered(4);
        assert!(queue.take_recovery_notice());
        assert!(!queue.take_recovery_notice());
        queue.enqueue(b"123456", false, false);
        queue.enqueue(b"q", false, false);
        assert!(queue.take_drop_notice());
        assert!(!queue.take_drop_notice());
        assert_eq!(rx.try_recv().unwrap(), b"abcd");
        assert_eq!(rx.try_recv().unwrap(), b"123456");
    }

    #[test]
    fn default_stall_threshold_uses_the_injected_clock() {
        let millis = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let clock_millis = std::sync::Arc::clone(&millis);
        let now: std::sync::Arc<dyn Fn() -> Duration + Send + Sync> =
            std::sync::Arc::new(move || {
                Duration::from_millis(clock_millis.load(std::sync::atomic::Ordering::SeqCst))
            });
        let (queue, _rx) = AttachInputQueue::with_clock(6, ATTACH_INPUT_STALL, now);
        assert!(matches!(
            queue.attempt(b"queued".to_vec()),
            super::AttachInputAttempt::Queued
        ));
        assert!(matches!(
            queue.attempt(b"x".to_vec()),
            super::AttachInputAttempt::Full(_)
        ));
        millis.store(2_999, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            queue.attempt(b"x".to_vec()),
            super::AttachInputAttempt::Full(_)
        ));
        millis.store(3_000, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            queue.attempt(b"x".to_vec()),
            super::AttachInputAttempt::Dropped
        ));
        assert!(queue.take_drop_notice());
    }

    #[test]
    fn dropping_during_bracketed_paste_forwards_the_closing_marker() {
        let (queue, rx) = AttachInputQueue::with_capacity(6, Duration::ZERO);
        queue.enqueue(b"\x1b[200~", false, true);
        queue.enqueue(b"paste body", true, false);
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[200~");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[201~");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dropped_paste_opener_discards_through_the_real_end_marker() {
        let (queue, rx) = AttachInputQueue::with_capacity(16, Duration::ZERO);
        queue.enqueue(b"0123456789", false, false);
        let mut mouse_on = false;
        let scrollback = super::AttachScrollback::default();
        let output_lock = std::sync::Mutex::new(());
        let mut route = AttachRoute {
            path: Path::new("unused"),
            name: "unused",
            input: &queue,
            mouse_on: &mut mouse_on,
            mouse_toggle_enabled: false,
            scrollback: &scrollback,
            output_lock: &output_lock,
            discarding_paste: false,
        };
        route_tokens(
            &mut route,
            vec![crate::mouse::InputToken::Paste(crate::mouse::PasteChunk {
                bytes: b"\x1b[200~ab".to_vec(),
                starts: true,
                ends: false,
            })],
        );
        assert!(route.discarding_paste);
        route_tokens(
            &mut route,
            vec![crate::mouse::InputToken::Paste(crate::mouse::PasteChunk {
                bytes: b"cd\n\x1b[201~".to_vec(),
                starts: false,
                ends: true,
            })],
        );
        assert!(!route.discarding_paste);
        queue.writer().delivered(10);
        route_tokens(
            &mut route,
            vec![crate::mouse::InputToken::Bytes(b"typed\n".to_vec())],
        );
        assert_eq!(rx.try_recv().unwrap(), b"0123456789");
        assert_eq!(rx.try_recv().unwrap(), b"typed\n");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn detach_inside_paste_requires_a_lone_ctrl_backslash_after_idle() {
        let mut parser = crate::mouse::SgrParser::default();
        assert!(parser.feed(b"\x1b[200~payload").is_empty());
        assert_eq!(detach_offset(&parser, &[DETACH]), None);
        assert_eq!(detach_offset(&parser, &[b'x', DETACH]), None);
        std::thread::sleep(Duration::from_millis(1010));
        assert_eq!(detach_offset(&parser, &[DETACH]), Some(0));
        assert!(
            matches!(parser.finish().last(), Some(crate::mouse::InputToken::Paste(chunk)) if chunk.ends && chunk.bytes.ends_with(crate::mouse::PASTE_END))
        );
    }

    #[test]
    fn detach_byte_inside_single_read_paste_is_data() {
        let parser = crate::mouse::SgrParser::default();
        let paste = b"\x1b[200~ab\x1ccd\x1b[201~";
        assert_eq!(detach_offset(&parser, paste), None);
    }

    #[test]
    fn detach_byte_inside_paste_after_split_start_marker_is_data() {
        let mut parser = crate::mouse::SgrParser::default();
        assert!(parser.feed(b"\x1b[20").is_empty());
        let tail = b"0~ab\x1ccd\x1b[201~";
        assert_eq!(detach_offset(&parser, tail), None);
        assert!(parser
            .feed(tail)
            .iter()
            .all(|token| matches!(token, crate::mouse::InputToken::Paste(_))));
    }

    #[test]
    fn detach_after_paste_end_in_the_same_read_is_still_a_hotkey() {
        let mut parser = crate::mouse::SgrParser::default();
        assert!(parser.feed(b"\x1b[200~body").is_empty());
        let input = b"tail\x1b[201~x\x1c";
        assert_eq!(detach_offset(&parser, input), Some(input.len() - 1));
    }

    #[test]
    fn raw_mouse_disabled_recovery_closes_child_paste_after_timeout() {
        let mut parser = crate::mouse::SgrParser::default();
        assert!(parser.feed(b"\x1b[200~raw bytes").is_empty());
        let tokens = parser.finish();
        assert!(matches!(
            tokens.last(),
            Some(crate::mouse::InputToken::Paste(chunk)) if chunk.ends && chunk.bytes.ends_with(crate::mouse::PASTE_END)
        ));
    }

    #[cfg(unix)]
    fn assert_request_timeout(request: Request) {
        static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);
        const TIMEOUT: Duration = Duration::from_millis(300);
        const TEST_DEADLINE: Duration = Duration::from_secs(2);
        let path = std::env::temp_dir().join(format!(
            "remuda-timeout-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = ipc::listen(&path).expect("bind raw local listener");
        let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("accept");
            accepted_tx.send(()).expect("notify accepted");
            release_rx.recv().expect("release silent peer");
            drop(stream);
        });

        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let started = Instant::now();
            let result = request_with_timeout(&client_path, &request, TIMEOUT);
            result_tx
                .send((result, started.elapsed()))
                .expect("report timed request");
        });
        accepted_rx
            .recv_timeout(TEST_DEADLINE)
            .expect("client did not connect to raw listener");

        let result = result_rx.recv_timeout(TEST_DEADLINE);
        // Release the peer even on failure: this lets a broken implementation
        // that forgot to wake its blocked read finish before the assertion.
        release_tx.send(()).expect("release silent peer");
        server.join().expect("silent listener thread");
        client.join().expect("timed request thread");
        #[cfg(unix)]
        let _ = std::fs::remove_file(&path);

        let (result, elapsed) = result.expect("request_with_timeout exceeded test deadline");
        let error = result.expect_err("silent listener unexpectedly produced a reply");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            elapsed >= TIMEOUT && elapsed < TEST_DEADLINE,
            "timeout returned outside the expected window: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn request_timeout_wakes_a_peer_that_never_replies() {
        assert_request_timeout(Request::Version);
    }

    #[cfg(unix)]
    #[test]
    fn request_timeout_bounds_a_large_input_to_a_peer_that_never_reads() {
        assert_request_timeout(Request::Input {
            name: "target".into(),
            instance_id: "instance".into(),
            client_id: "client".into(),
            seq: 1,
            bytes: vec![b'x'; 64 * 1024],
        });
    }

    #[cfg(unix)]
    #[test]
    fn reply_reader_rejects_a_line_over_the_shared_reply_limit() {
        use super::read_response;
        use crate::ipc;
        use crate::reply_limit::max_reply_wire_bytes;
        use interprocess::local_socket::traits::Listener as _;
        use std::io::Write;

        let path = std::path::PathBuf::from(format!(
            "/tmp/r-reply-{}-{}.sock",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&path);
        let listener = ipc::listen(&path).unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut line = vec![b'x'; max_reply_wire_bytes() + 1];
            line.push(b'\n');
            let _ = stream.write_all(&line);
        });
        let stream = ipc::connect(&path).unwrap();
        let result = read_response(&stream);
        drop(stream);
        server.join().unwrap();
        let _ = std::fs::remove_file(path);

        assert!(
            result.is_err(),
            "the client must reject an oversized reply line before parsing it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn response_is_read_when_peer_closes_before_timeout_is_set() {
        use crate::ipc;
        use std::io::Write;

        let path = std::env::temp_dir().join(format!(
            "remuda-client-close-before-timeout-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = ipc::listen(&path).unwrap();
        let expected = Response::Value("immediate".into());
        let server = std::thread::spawn({
            let expected = expected.clone();
            move || {
                let mut stream = listener.accept().unwrap();
                let mut line = serde_json::to_vec(&expected).unwrap();
                line.push(b'\n');
                stream.write_all(&line).unwrap();
            }
        });
        let stream = ipc::connect(&path).unwrap();
        server.join().unwrap();

        let response = read_response_with_timeout(&path, stream, Duration::from_secs(1)).unwrap();

        assert_eq!(response, expected);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn detach_resets_mouse_and_bracketed_paste_modes() {
        let mut output = Vec::new();
        reset_input_modes(&mut output).unwrap();
        assert_eq!(output, RESET_INPUT_MODES);
    }

    #[cfg(unix)]
    #[test]
    fn local_daemon_response_wait_has_a_timeout() {
        use interprocess::local_socket::traits::ListenerExt as _;
        let path = std::env::temp_dir().join(format!(
            "remuda-client-timeout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = crate::ipc::listen(&path).unwrap();
        let server = std::thread::spawn(move || {
            let _stream = listener.incoming().next().unwrap().unwrap();
            std::thread::sleep(Duration::from_millis(250));
        });
        let started = std::time::Instant::now();
        let error =
            request_with_timeout(&path, &Request::List, Duration::from_millis(40)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(200));
        server.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn input_trace_records_a_timestamp_and_each_byte_as_hex() {
        let mut output = Vec::new();
        // Windows SystemTime has 100 ns precision, so keep the fixture on that
        // clock's representable grid as well.
        let at = UNIX_EPOCH + Duration::new(7, 420_000_000);
        write_input_trace(&mut output, at, b"\x1b\xff").unwrap();
        assert_eq!(output, b"7.420000000 1b ff\n");
    }

    #[cfg(unix)]
    #[test]
    fn input_trace_is_private_when_created_and_when_reusing_a_loose_file() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "remuda-input-trace-mode-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&path);

        trace_input_read(Some(&path), b"secret");
        let private_mode = || std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(private_mode() & 0o077, 0, "new trace file must be private");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        trace_input_read(Some(&path), b"secret again");
        assert_eq!(
            private_mode() & 0o077,
            0,
            "existing trace file must be made private"
        );
        let _ = std::fs::remove_file(path);
    }

    /// The line the daemon at 618cda4^ actually sent, byte for byte.
    const STALE: &str =
        r#"{"Error":"bad request: invalid type: null, expected a string at line 1 column 19"}"#;

    #[test]
    fn a_daemon_that_cannot_read_us_is_named_as_a_stale_build() {
        let Response::Error(said) = interpret(STALE) else {
            panic!("not an error")
        };
        assert!(said.starts_with("the daemon is not this build"), "{said}");
        // The serde detail is the real diagnostic; it must survive, just not lead.
        assert!(said.contains("invalid type: null"), "{said}");
    }

    #[test]
    fn an_unknown_variant_is_the_same_diagnosis() {
        // What an old daemon says to `Version` — the shape step 015 must not
        // turn into something more confusing than what it replaced.
        let line = r#"{"Error":"bad request: unknown variant `Version`, expected one of `List`, `New` at line 1 column 11"}"#;
        let Response::Error(said) = interpret(line) else {
            panic!("not an error")
        };
        assert!(said.starts_with("the daemon is not this build"), "{said}");
    }

    /// The TUI footer is one row wide and crops the tail, so a message that
    /// leads with the diagnosis loses the cure off the right edge. Measured at
    /// 80 columns with the 8-character "remuda: " prefix a caller adds.
    #[test]
    fn the_cure_survives_an_eighty_column_crop() {
        let Response::Error(said) = interpret(STALE) else {
            panic!("not an error")
        };
        let footer: String = format!("remuda: {said}").chars().take(80).collect();
        assert!(
            footer.contains("remuda stop"),
            "cropped away the cure:\n{footer}"
        );
    }

    /// The negative control: a refusal about CONTENT is not skew and must reach
    /// the user in the daemon's own words. Without this the fix above would
    /// swallow every error into one sentence and be indistinguishable from it.
    #[test]
    fn an_ordinary_refusal_is_passed_through_untouched() {
        let line = r#"{"Error":"no such session: build"}"#;
        assert_eq!(
            interpret(line),
            Response::Error("no such session: build".into())
        );
    }

    #[test]
    fn a_hangup_is_not_reported_as_a_parse_failure() {
        let Response::Error(said) = interpret("") else {
            panic!("not an error")
        };
        assert!(said.contains("hung up"), "{said}");
    }

    #[test]
    fn a_normal_reply_still_arrives_as_itself() {
        assert_eq!(interpret(r#""Ok""#), Response::Ok);
    }

    #[test]
    fn scrollback_indicator_truncates_at_terminal_display_width() {
        let indicator = "[scrollback: 24 rows · q/Esc exit · keys go live]";
        let short = truncate_terminal_text(indicator, 40);
        assert_eq!(unicode_width::UnicodeWidthStr::width(short.as_str()), 40);
        assert_eq!(truncate_terminal_text(indicator, 80), indicator);
    }

    #[cfg(unix)]
    #[test]
    fn exit_key_is_forwarded_if_another_action_returned_live_under_the_lock() {
        let daemon_path =
            std::env::temp_dir().join(format!("r-no-daemon-{}.sock", std::process::id()));
        let scrollback = std::sync::Arc::new(super::AttachScrollback::default());
        scrollback.offset.store(3, Ordering::SeqCst);
        let output_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
        let held_lock = output_lock.lock().unwrap();
        let (queue, input_rx) = AttachInputQueue::with_capacity(64, Duration::ZERO);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn({
            let scrollback = std::sync::Arc::clone(&scrollback);
            let output_lock = std::sync::Arc::clone(&output_lock);
            let daemon_path = daemon_path.clone();
            move || {
                let mut mouse_on = false;
                let mut route = AttachRoute {
                    path: &daemon_path,
                    name: "target",
                    input: &queue,
                    mouse_on: &mut mouse_on,
                    mouse_toggle_enabled: false,
                    scrollback: &scrollback,
                    output_lock: &output_lock,
                    discarding_paste: false,
                };
                started_tx.send(()).expect("signal exit-key routing");
                super::exit_history_if_needed(&mut route, b"q");
            }
        });
        started_rx.recv().expect("exit-key worker started");
        std::thread::sleep(Duration::from_millis(50));
        scrollback.offset.store(0, Ordering::SeqCst);
        drop(held_lock);
        worker.join().expect("exit-key worker");
        assert_eq!(input_rx.try_recv().unwrap(), b"q");
    }

    #[cfg(unix)]
    #[test]
    fn paste_while_scrolled_returns_live_and_reaches_child_unchanged() {
        use crate::mouse::{InputToken, PasteChunk};

        let daemon_path =
            std::env::temp_dir().join(format!("r-no-daemon-{}.sock", std::process::id()));
        let scrollback = super::AttachScrollback::default();
        scrollback.offset.store(7, Ordering::SeqCst);
        let paste = b"\x1b[200~typed paste\x1b[201~".to_vec();
        let (queue, input_rx) = AttachInputQueue::with_capacity(1024, Duration::ZERO);
        let mut mouse_on = false;
        let output_lock = std::sync::Mutex::new(());
        let mut route = AttachRoute {
            path: &daemon_path,
            name: "target",
            input: &queue,
            mouse_on: &mut mouse_on,
            mouse_toggle_enabled: false,
            scrollback: &scrollback,
            output_lock: &output_lock,
            discarding_paste: false,
        };
        super::route_tokens(
            &mut route,
            vec![InputToken::Paste(PasteChunk {
                bytes: paste.clone(),
                starts: true,
                ends: true,
            })],
        );
        let received: Vec<u8> = input_rx.try_iter().flatten().collect();
        assert_eq!(scrollback.offset.load(Ordering::SeqCst), 0);
        assert_eq!(received, paste);
    }
}
