//! A real agent: a process on a real pty, with its screen kept as a grid.
//!
//! # Why a terminal emulator is not optional here
//!
//! A coding agent redraws its screen. Accumulating raw pty bytes gives you the
//! *history of the drawing*, not the picture — and `Cursor` is unobtainable
//! from raw bytes at all. Ghost/autocomplete text is told from real typed input
//! **by cursor column**, so ⚠ cursor queryability is a hard requirement on
//! whatever parses the stream, not a nice-to-have.
//!
//! # Why `vt100` and not `termwiz`
//!
//! `vt100` offers exactly the three things [`AgentProcess`] asks for — feed
//! bytes, read the grid, read the cursor — in one crate, whereas termwiz's
//! equivalent read path is documented as existing *"primarily for testing"*. A
//! pty hands over bytes, so there is no compatibility surface between the two
//! crates to keep aligned. Reversible on purpose: the choice sits behind
//! [`AgentProcess`], the seam built so the vendor can change without touching
//! a line of policy.

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use remuda_core::agent::{
    AgentError, AgentProcess, AgentWriter, ChainOutcome, Color, Cursor, ExitInfo, MouseEncoding,
    MouseMode, MouseState, OutputSignal, OutputWakeup, Result, ScreenSnapshot, Size, StyledCell,
    VersionedSnapshot,
};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Live viewers of one pty's output. Shared with the reader thread, which is
/// the only producer; every consumer holds the other end of a channel.
enum Watcher {
    Bytes(Sender<Vec<u8>>),
    Wake(OutputSignal),
}

type Watchers = Arc<Mutex<Vec<Watcher>>>;

/// The pty's input end. Shared, because the reader thread must answer the
/// terminal's own questions — see [`DSR_CURSOR`].
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub const PTY_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PTY_LATE_SUBMIT_BOUND: Duration = Duration::from_secs(30);

struct WriteTask {
    sequence: u64,
    bytes: Vec<u8>,
    result: Sender<Result<()>>,
}

#[derive(Default)]
struct WriterState {
    next_sequence: u64,
    active_sequence: Option<u64>,
    stalled_sequence: Option<u64>,
    active_since: Option<Instant>,
    follow_up: Option<(Vec<u8>, Duration)>,
    follow_up_open: bool,
    late_submit_abandoned: bool,
}

/// One bounded worker owns blocking PTY writes. It accepts only one task at a
/// time and never holds the process or screen lock while its write blocks.
struct PtyInputWriter {
    sender: SyncSender<WriteTask>,
    state: Arc<Mutex<WriterState>>,
    timeout: Duration,
    late_submit_bound: Duration,
}

impl PtyInputWriter {
    fn spawn(
        writer: SharedWriter,
        timeout: Duration,
        late_submit_bound: Duration,
    ) -> std::io::Result<Self> {
        let (sender, receiver) = sync_channel::<WriteTask>(1);
        let state = Arc::new(Mutex::new(WriterState::default()));
        let worker_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("remuda-pty-writer".into())
            .spawn(move || run_writer(receiver, writer, worker_state, late_submit_bound))?;
        Ok(Self {
            sender,
            state,
            timeout,
            late_submit_bound,
        })
    }

    fn submit(&self, bytes: &[u8]) -> Result<(u64, Receiver<Result<()>>)> {
        self.submit_inner(bytes, false)
    }

    fn submit_bounded(&self, bytes: &[u8]) -> Result<(u64, Receiver<Result<()>>)> {
        self.submit_inner(bytes, true)
    }

    fn submit_inner(
        &self,
        bytes: &[u8],
        report_completed_abandonment: bool,
    ) -> Result<(u64, Receiver<Result<()>>)> {
        let sequence = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.active_since.is_some() {
                return Err(AgentError::Busy);
            }
            if report_completed_abandonment && state.late_submit_abandoned {
                state.late_submit_abandoned = false;
                return Err(AgentError::LateSubmitAbandoned {
                    bound: self.late_submit_bound,
                });
            }
            state.next_sequence = state.next_sequence.wrapping_add(1);
            let sequence = state.next_sequence;
            state.active_sequence = Some(sequence);
            state.active_since = Some(Instant::now());
            state.follow_up = None;
            state.follow_up_open = true;
            sequence
        };
        let (result, receiver) = channel();
        let task = WriteTask {
            sequence,
            bytes: bytes.to_vec(),
            result,
        };
        match self.sender.try_send(task) {
            Ok(()) => Ok((sequence, receiver)),
            Err(TrySendError::Full(_)) => {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                state.active_sequence = None;
                state.active_since = None;
                state.follow_up = None;
                state.follow_up_open = false;
                Err(AgentError::Busy)
            }
            Err(TrySendError::Disconnected(_)) => {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                state.active_sequence = None;
                state.active_since = None;
                state.follow_up = None;
                state.follow_up_open = false;
                Err(AgentError::Io("pty writer worker stopped".into()))
            }
        }
    }

    fn record_stalled_sequence(&self, sequence: u64) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.stalled_sequence = Some(sequence);
    }

    fn note_late_submit_abandoned(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let past_bound = state
            .active_since
            .is_some_and(|started| started.elapsed() >= self.late_submit_bound);
        if past_bound && state.follow_up.is_some() {
            state.follow_up = None;
            state.follow_up_open = false;
            state.late_submit_abandoned = true;
        }
        state.active_since.is_some() && state.late_submit_abandoned
    }
}

fn run_writer(
    receiver: Receiver<WriteTask>,
    writer: SharedWriter,
    state: Arc<Mutex<WriterState>>,
    late_submit_bound: Duration,
) {
    while let Ok(task) = receiver.recv() {
        let reset_busy = BusyReset(&state);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let result = match writer.lock() {
                Ok(mut writer) => writer
                    .write_all(&task.bytes)
                    .and_then(|()| writer.flush())
                    .map_err(io),
                Err(_) => Err(io("pty writer lock poisoned")),
            };
            let follow_up = {
                let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
                state.follow_up_open = false;
                let too_late = state
                    .active_since
                    .is_some_and(|started| started.elapsed() >= late_submit_bound);
                if result.is_ok() && state.active_sequence == Some(task.sequence) && !too_late {
                    state.follow_up.take()
                } else {
                    if result.is_ok()
                        && state.active_sequence == Some(task.sequence)
                        && too_late
                        && state.follow_up.is_some()
                    {
                        state.late_submit_abandoned = true;
                    }
                    state.follow_up = None;
                    None
                }
            };
            if let Some(follow_up) = follow_up {
                let (bytes, settle) = follow_up;
                if !settle.is_zero() {
                    std::thread::sleep(settle);
                }
                match writer.lock() {
                    Ok(mut writer) => writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .map_err(io)?,
                    Err(_) => return Err(io("pty writer lock poisoned")),
                }
            }
            result
        }))
        .unwrap_or_else(|_| Err(io("pty writer panicked")));
        drop(reset_busy);
        let _ = task.result.send(result);
    }
}

struct BusyReset<'a>(&'a Mutex<WriterState>);

impl Drop for BusyReset<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.active_sequence = None;
        state.active_since = None;
        state.follow_up = None;
        state.follow_up_open = false;
    }
}

impl AgentWriter for PtyInputWriter {
    fn chain_after_stalled(&self, follow_up: &[u8], settle: Duration) -> ChainOutcome {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(active_sequence) = state.active_sequence else {
            return ChainOutcome::Landed;
        };
        if Some(active_sequence) != state.stalled_sequence || !state.follow_up_open {
            return ChainOutcome::Landed;
        }
        let too_late = state
            .active_since
            .is_some_and(|started| started.elapsed() >= self.late_submit_bound);
        if too_late {
            state.follow_up = None;
            state.follow_up_open = false;
            state.late_submit_abandoned = true;
            return ChainOutcome::Unsupported;
        }
        state.follow_up = Some((follow_up.to_vec(), settle));
        ChainOutcome::Chained
    }

    fn write_bounded(&self, bytes: &[u8]) -> Result<()> {
        let (sequence, receiver) = loop {
            match self.submit_bounded(bytes) {
                Ok(submission) => break submission,
                Err(AgentError::Busy) if self.note_late_submit_abandoned() => {
                    return Err(AgentError::LateSubmitAbandoned {
                        bound: self.late_submit_bound,
                    });
                }
                Err(AgentError::Busy) if self.is_timed_out() => {
                    // This request never reached the PTY worker. Leave it
                    // retryable; only a timeout of our submitted receiver is
                    // ambiguous to the caller.
                    return Err(AgentError::Busy);
                }
                Err(AgentError::Busy) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => return Err(error),
            }
        };
        match receiver.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.record_stalled_sequence(sequence);
                Err(AgentError::WriteTimeout {
                    timeout: self.timeout,
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(AgentError::Io("pty writer worker stopped".into()))
            }
        }
    }

    fn write_to_completion(&self, bytes: &[u8]) -> Result<()> {
        self.write_to_completion_while(bytes, &|| false)
    }

    fn write_to_completion_while(&self, bytes: &[u8], cancelled: &dyn Fn() -> bool) -> Result<()> {
        loop {
            if cancelled() {
                return Err(AgentError::Attached);
            }
            match self.submit(bytes) {
                Ok((_, receiver)) => loop {
                    if cancelled() {
                        return Err(AgentError::Attached);
                    }
                    match receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(result) => return result,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            return Err(AgentError::Io("pty writer worker stopped".into()));
                        }
                    }
                },
                Err(AgentError::Busy) if self.is_timed_out() => return Err(AgentError::Busy),
                Err(AgentError::Busy) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => return Err(error),
            }
        }
    }

    fn is_busy(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active_since
            .is_some()
    }

    fn is_timed_out(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active_since
            .is_some_and(|started| started.elapsed() >= self.timeout)
    }
}

/// "Where is the cursor?" — a query the terminal must answer. ⚠ ConPTY asks it
/// BEFORE emitting anything and waits: unanswered, the child is alive and the
/// screen is blank forever. Measured on a windows-latest runner; see steps/010.
const DSR_CURSOR: &[u8] = b"\x1b[6n";
const SCROLLBACK_ROWS: usize = 10_000;
const SCROLLBACK_PROBE_CHUNK: usize = 512;

fn io<E: std::fmt::Display>(e: E) -> AgentError {
    AgentError::Io(e.to_string())
}

/// Temporarily selects a history viewport while guaranteeing the parser's
/// live viewport is restored, including while unwinding from a panic.
fn with_scrollback<T>(
    screen: &mut vt100::Screen,
    scrollback: usize,
    read: impl FnOnce(&vt100::Screen) -> T,
) -> T {
    let previous = screen.scrollback();
    screen.set_scrollback(scrollback);
    let restore = ScrollbackRestore { screen, previous };
    read(&*restore.screen)
}

struct ScrollbackRestore<'a> {
    screen: &'a mut vt100::Screen,
    previous: usize,
}

impl Drop for ScrollbackRestore<'_> {
    fn drop(&mut self) {
        self.screen.set_scrollback(self.previous);
    }
}

/// `vt100::Color` and [`Color`] are shaped identically on purpose; this is
/// the one place that fact is spent.
fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Default,
        vt100::Color::Idx(n) => Color::Idx(n),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// A child process attached to a pty, with its screen parsed into a grid.
pub struct PtyAgent {
    size: Size,
    screen: Arc<Mutex<vt100::Parser>>,
    input_writer: Arc<PtyInputWriter>,
    child: Box<dyn Child + Send + Sync>,
    watchers: Watchers,
    reader_closed: Arc<AtomicBool>,
    scrollback_total: Arc<AtomicUsize>,
    output_version: Arc<AtomicU64>,
    exit_info: Option<ExitInfo>,
    master: Option<Box<dyn MasterPty + Send>>,
    /// Windows: what makes a process this session's own after its parent
    /// has exited. Held for as long as the session is listed.
    #[cfg(windows)]
    job: crate::session_job::SessionJob,
}

/// Windows: the session's job, holding the child and what it has already
/// started. If that cannot be done the child is killed and the session is
/// refused: its orphans could not be told from outside processes.
#[cfg(windows)]
fn session_job_for(
    child: &mut (dyn Child + Send + Sync),
) -> Result<crate::session_job::SessionJob> {
    use crate::session_job::{set_up, SessionJob};
    let handle = child.as_raw_handle();
    let pid = child.process_id();
    let assign = |job: &SessionJob| {
        let (Some(handle), Some(pid)) = (handle, pid) else {
            return Err(std::io::Error::other("the child has no process handle"));
        };
        job.assign(handle)?;
        job.sweep(pid, handle)
    };
    let kill = || {
        let _ = child.kill();
    };
    set_up(SessionJob::new(), assign, kill).map_err(AgentError::Io)
}

impl PtyAgent {
    /// Spawn `command` on a new pty of `size`.
    pub fn spawn(command: CommandBuilder, size: Size) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: size.rows(),
                cols: size.cols(),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io)?;

        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut child = pair.slave.spawn_command(command).map_err(io)?;
        #[cfg(windows)]
        let job = session_job_for(child.as_mut())?;
        // No child_guard here on purpose — this child already dies with the
        // daemon by kernel accident (the master fd closes on any daemon
        // exit, SIGHUP-ing this session leader). See child_guard.rs and
        // native/tests/pty_survives_daemon_death.rs, which pins it. On Windows
        // the session's job (kill on close) ends the child and what it started.
        crate::child_guard::documented_pty_hangup_accident();
        drop(pair.slave); // Or the master never sees EOF when the child exits.

        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer().map_err(io)?));
        let input_writer = Arc::new(
            PtyInputWriter::spawn(
                Arc::clone(&writer),
                PTY_WRITE_TIMEOUT,
                PTY_LATE_SUBMIT_BOUND,
            )
            .map_err(io)?,
        );
        let reader = pair.master.try_clone_reader().map_err(io)?;
        let screen = Arc::new(Mutex::new(vt100::Parser::new(
            size.rows(),
            size.cols(),
            SCROLLBACK_ROWS,
        )));
        let watchers: Watchers = Arc::new(Mutex::new(Vec::new()));
        let reader_closed = Arc::new(AtomicBool::new(false));
        let scrollback_total = Arc::new(AtomicUsize::new(0));
        let output_version = Arc::new(AtomicU64::new(0));
        spawn_reader(
            reader,
            Arc::clone(&screen),
            Arc::clone(&watchers),
            Arc::clone(&reader_closed),
            Arc::clone(&writer),
            Arc::clone(&scrollback_total),
            Arc::clone(&output_version),
        );

        Ok(Self {
            size,
            screen,
            input_writer,
            child,
            watchers,
            reader_closed,
            scrollback_total,
            output_version,
            exit_info: None,
            master: Some(pair.master),
            #[cfg(windows)]
            job,
        })
    }
}

/// Drain the pty into the grid until EOF, on its own thread, tee-ing every chunk
/// to live watchers — a machine polls the grid, a human needs bytes as they
/// come. A hung-up subscriber is dropped on its next failed send.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
    watchers: Watchers,
    reader_closed: Arc<AtomicBool>,
    writer: SharedWriter,
    scrollback_total: Arc<AtomicUsize>,
    output_version: Arc<AtomicU64>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            let asked = buf[..n].windows(DSR_CURSOR.len()).any(|w| w == DSR_CURSOR);
            let at = match screen.lock() {
                Ok(mut parser) => {
                    let at = process_output(&mut parser, &buf[..n], &scrollback_total);
                    output_version.fetch_add(1, Ordering::SeqCst);
                    at
                }
                // Poisoned: the grid can no longer be trusted.
                Err(_) => break,
            };
            if asked {
                answer_cursor_query(&writer, at);
            }
            if let Ok(mut watchers) = watchers.lock() {
                watchers.retain(|watcher| match watcher {
                    Watcher::Bytes(sender) => sender.send(buf[..n].to_vec()).is_ok(),
                    Watcher::Wake(signal) => signal.wake(),
                });
            }
        }
        // EOF: drop every sender so each attached viewer's recv() ends instead
        // of blocking forever on a process that is gone.
        if let Ok(mut watchers) = watchers.lock() {
            watchers.clear();
            reader_closed.store(true, Ordering::Release);
        } else {
            reader_closed.store(true, Ordering::Release);
        }
    });
}

/// Process bounded chunks while a temporary nonzero scroll offset counts
/// every full-screen row pushed into history, even when the retained buffer is full.
fn process_output(
    parser: &mut vt100::Parser,
    bytes: &[u8],
    scrollback_total: &AtomicUsize,
) -> (u16, u16) {
    for chunk in bytes.chunks(SCROLLBACK_PROBE_CHUNK) {
        let screen = parser.screen_mut();
        let previous = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let before_len = screen.scrollback();
        screen.set_scrollback(1);
        let probe_start = screen.scrollback();

        parser.process(chunk);

        let screen = parser.screen_mut();
        let probe_end = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let after_len = screen.scrollback();
        screen.set_scrollback(previous);

        let scrolled = if probe_start == 0 {
            after_len.saturating_sub(before_len)
        } else {
            probe_end.saturating_sub(probe_start)
        };
        let _ = scrollback_total.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
            Some(total.saturating_add(scrolled))
        });
    }
    let (row, col) = display_cursor(parser.screen());
    (row + 1, col + 1)
}

/// Reply to a cursor-position query when the writer is free. The reader never
/// waits behind a stalled input write; a busy child can ask again later.
// ponytail: matched within one read. ConPTY writes the query as a single
// four-byte message; a split one would be missed until the next ask.
fn answer_cursor_query(writer: &SharedWriter, (row, col): (u16, u16)) {
    let reply = format!("\x1b[{row};{col}R");
    if let Ok(mut writer) = writer.try_lock() {
        let _ = writer.write_all(reply.as_bytes());
        let _ = writer.flush();
    }
}

/// The cursor as a terminal shows and reports it: vt100 leaves it one past the
/// last column in DECAWM pending wrap, where xterm/iTerm/tmux draw and report
/// (DSR 6n) the last column, so clamp to `cols - 1`.
pub(crate) fn display_cursor(screen: &vt100::Screen) -> (u16, u16) {
    let (row, col) = screen.cursor_position();
    (row, col.min(screen.size().1.saturating_sub(1)))
}

pub(crate) fn styled_cells(screen: &vt100::Screen, size: Size) -> Vec<Vec<StyledCell>> {
    (0..size.rows())
        .map(|row| {
            (0..size.cols())
                .map(|col| {
                    let cell = screen.cell(row, col);
                    let continuation = cell.is_some_and(|c| c.is_wide_continuation());
                    let text = if continuation {
                        String::new()
                    } else {
                        let t = cell.map_or("", |c| c.contents());
                        (if t.is_empty() { " " } else { t }).to_string()
                    };
                    StyledCell {
                        text,
                        wide: cell.is_some_and(|c| c.is_wide()),
                        fg: color(cell.map_or(vt100::Color::Default, |c| c.fgcolor())),
                        bg: color(cell.map_or(vt100::Color::Default, |c| c.bgcolor())),
                        bold: cell.is_some_and(|c| c.bold()),
                        dim: cell.is_some_and(|c| c.dim()),
                        italic: cell.is_some_and(|c| c.italic()),
                        underline: cell.is_some_and(|c| c.underline()),
                        inverse: cell.is_some_and(|c| c.inverse()),
                    }
                })
                .collect()
        })
        .collect()
}

impl AgentProcess for PtyAgent {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.is_alive() {
            return Err(AgentError::Exited);
        }
        self.input_writer.write_bounded(bytes)
    }

    fn input_writer(&mut self) -> Option<Arc<dyn AgentWriter>> {
        Some(Arc::clone(&self.input_writer) as Arc<dyn AgentWriter>)
    }

    fn screen_text(&mut self) -> Result<String> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(parser.screen().contents())
    }

    /// The screen and input modes as terminal bytes, cursor included.
    /// `state_formatted` emits a full repaint plus mouse, paste, and keypad
    /// modes, which a just-attached terminal needs even if the child is idle.
    fn screen_bytes(&mut self) -> Result<Vec<u8>> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(parser.screen().state_formatted())
    }

    fn mouse_tracking(&mut self) -> bool {
        self.screen
            .lock()
            .map(|parser| parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None)
            .unwrap_or(false)
    }

    fn mouse_state(&mut self) -> MouseState {
        self.screen
            .lock()
            .map(|parser| {
                let screen = parser.screen();
                MouseState {
                    mode: match screen.mouse_protocol_mode() {
                        vt100::MouseProtocolMode::None => MouseMode::None,
                        vt100::MouseProtocolMode::Press => MouseMode::Press,
                        vt100::MouseProtocolMode::PressRelease => MouseMode::PressRelease,
                        vt100::MouseProtocolMode::ButtonMotion => MouseMode::ButtonMotion,
                        vt100::MouseProtocolMode::AnyMotion => MouseMode::AnyMotion,
                    },
                    encoding: match screen.mouse_protocol_encoding() {
                        vt100::MouseProtocolEncoding::Default => MouseEncoding::Default,
                        vt100::MouseProtocolEncoding::Utf8 => MouseEncoding::Utf8,
                        vt100::MouseProtocolEncoding::Sgr => MouseEncoding::Sgr,
                    },
                    bracketed_paste: screen.bracketed_paste(),
                }
            })
            .unwrap_or_default()
    }

    /// Styled cells off `vt100`'s own grid, including its wide/continuation
    /// judgment (`is_wide`/`is_wide_continuation`) rather than re-deriving
    /// it — this is the path that actually keeps colour. See steps/020, 023.
    fn screen_cells(&mut self) -> Result<Vec<Vec<StyledCell>>> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(styled_cells(parser.screen(), self.size))
    }

    fn screen_cells_at(&mut self, scrollback: usize) -> Result<Vec<Vec<StyledCell>>> {
        let mut parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(with_scrollback(parser.screen_mut(), scrollback, |screen| {
            styled_cells(screen, self.size)
        }))
    }

    fn scrollback_len(&mut self) -> usize {
        let Ok(mut parser) = self.screen.lock() else {
            return 0;
        };
        let screen = parser.screen_mut();
        let previous = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let length = screen.scrollback();
        screen.set_scrollback(previous);
        length
    }

    fn scrollback_total(&mut self) -> usize {
        self.scrollback_total.load(Ordering::Relaxed)
    }

    fn row_wrapped_at(&mut self, scrollback: usize) -> Result<Vec<bool>> {
        let mut parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        Ok(with_scrollback(parser.screen_mut(), scrollback, |screen| {
            (0..self.size.rows())
                .map(|row| screen.row_wrapped(row))
                .collect()
        }))
    }

    fn screen_snapshot_at(&mut self, scrollback: usize) -> Result<VersionedSnapshot> {
        capture_snapshot(
            &self.screen,
            self.size,
            &self.scrollback_total,
            &self.output_version,
            scrollback,
        )
    }

    fn output_version(&mut self) -> Option<u64> {
        Some(self.output_version.load(Ordering::SeqCst))
    }

    fn subscribe(&mut self) -> Option<Receiver<Vec<u8>>> {
        let (tx, rx) = channel();
        let mut watchers = self.watchers.lock().ok()?;
        if !self.reader_closed.load(Ordering::Acquire) {
            watchers.push(Watcher::Bytes(tx));
        }
        Some(rx)
    }

    fn subscribe_output_wakeup(&mut self) -> Option<OutputWakeup> {
        let (signal, wakeup) = OutputWakeup::pair(Arc::clone(&self.output_version));
        let mut watchers = self.watchers.lock().ok()?;
        if !self.reader_closed.load(Ordering::Acquire) {
            watchers.push(Watcher::Wake(signal));
        }
        Some(wakeup)
    }

    fn cursor(&mut self) -> Result<Cursor> {
        let parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        let screen = parser.screen();
        let (row, col) = display_cursor(screen);
        Ok(Cursor {
            row,
            col,
            visible: !screen.hide_cursor(),
        })
    }

    fn is_alive(&mut self) -> bool {
        let alive = match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.record_exit_status(status);
                false
            }
            Err(_) => false,
        };
        if !alive {
            // ConPTY keeps its output pipe open after the child exits until
            // ClosePseudoConsole runs. Release the master so the reader thread
            // sees EOF and attached clients receive the ordinary end-of-stream.
            self.master.take();
        }
        alive
    }

    fn exit_info(&mut self) -> Option<ExitInfo> {
        self.exit_info.clone()
    }

    /// Caution: `is_alive` calls `try_wait`, which *reaps* the child on unix, so
    /// `kill()` afterwards fails with ESRCH. The trait's idempotence is
    /// therefore explicit here — an already-gone process is nothing to signal.
    fn terminate(&mut self) -> Result<()> {
        if !self.is_alive() {
            return Ok(());
        }
        self.child.kill().map_err(io)?;
        let status = self.child.wait().map_err(io)?;
        self.record_exit_status(status);
        // ConPTY can keep the output reader alive after the child exits until
        // ClosePseudoConsole runs. Close the master here so output monitors
        // can flush before the caller waits for their final notification.
        self.master.take();
        Ok(())
    }

    fn resize(&mut self, size: Size) -> Result<()> {
        self.master
            .as_ref()
            .ok_or(AgentError::Exited)?
            .resize(PtySize {
                rows: size.rows(),
                cols: size.cols(),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io)?;
        let mut parser = self.screen.lock().map_err(|_| io("screen lock poisoned"))?;
        parser.screen_mut().set_size(size.rows(), size.cols());
        self.output_version.fetch_add(1, Ordering::SeqCst);
        self.size = size;
        Ok(())
    }

    fn size(&self) -> Size {
        self.size
    }

    fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    #[cfg(windows)]
    fn owns_process(&self, pid: u32) -> bool {
        self.job.contains(pid)
    }
}

#[cfg(test)]
mod input_writer_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    struct StalledWrite {
        started: Mutex<Option<Sender<()>>>,
        release: Mutex<Receiver<()>>,
        finished: Sender<()>,
        first: AtomicBool,
        captured: Arc<Mutex<Vec<u8>>>,
    }

    struct SequentialStallWrite {
        started: Sender<Vec<u8>>,
        release: Mutex<Receiver<()>>,
        captured: Arc<Mutex<Vec<u8>>>,
    }

    struct PanickingWrite;

    impl Write for PanickingWrite {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            panic!("injected PTY writer panic");
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct CaptureWrites(Arc<Mutex<Vec<u8>>>);

    impl Write for CaptureWrites {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Write for StalledWrite {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.first.swap(true, Ordering::AcqRel) {
                self.started
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                self.release.lock().unwrap().recv().unwrap();
                self.finished.send(()).unwrap();
            }
            self.captured.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Write for SequentialStallWrite {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.started.send(bytes.to_vec()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            self.captured.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn timed_out_worker_keeps_one_busy_write_then_recovers() {
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let (finished_tx, finished_rx) = channel();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(StalledWrite {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(release_rx),
            finished: finished_tx,
            first: AtomicBool::new(false),
            captured,
        })));
        let writer = Arc::new(
            PtyInputWriter::spawn(writer, Duration::from_millis(100), PTY_LATE_SUBMIT_BOUND)
                .unwrap(),
        );
        let first_writer = Arc::clone(&writer);
        let first = std::thread::spawn(move || first_writer.write_bounded(b"first"));
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert!(matches!(
            first.join().unwrap(),
            Err(AgentError::WriteTimeout { .. })
        ));
        assert!(writer.is_busy());
        assert!(matches!(
            writer.write_bounded(b"not submitted"),
            Err(AgentError::Busy)
        ));

        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while writer.is_busy() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(!writer.is_busy());
        writer
            .write_bounded(b"retried after no-submit Busy")
            .unwrap();
    }

    #[test]
    fn timed_out_worker_chains_follow_up_before_accepting_another_write() {
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let (finished_tx, finished_rx) = channel();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(StalledWrite {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(release_rx),
            finished: finished_tx,
            first: AtomicBool::new(false),
            captured: Arc::clone(&captured),
        })));
        let writer = Arc::new(
            PtyInputWriter::spawn(writer, Duration::from_millis(100), PTY_LATE_SUBMIT_BOUND)
                .unwrap(),
        );

        assert_eq!(
            writer.chain_after_stalled(b"\r", Duration::ZERO),
            ChainOutcome::Landed
        );
        let first_writer = Arc::clone(&writer);
        let first = std::thread::spawn(move || first_writer.write_bounded(b"text"));
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            first.join().unwrap(),
            Err(AgentError::WriteTimeout { .. })
        ));

        let settle = Duration::from_millis(25);
        assert_eq!(
            writer.chain_after_stalled(b"\r", settle),
            ChainOutcome::Chained
        );
        assert!(matches!(
            writer.write_bounded(b"second sender"),
            Err(AgentError::Busy)
        ));

        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while writer.is_busy() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(*captured.lock().unwrap(), b"text\r");
        assert!(!writer.is_busy());
        assert_eq!(
            writer.chain_after_stalled(b"\r", Duration::ZERO),
            ChainOutcome::Landed
        );
    }

    #[test]
    fn chain_after_stalled_does_not_follow_a_later_write() {
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(SequentialStallWrite {
            started: started_tx,
            release: Mutex::new(release_rx),
            captured: Arc::clone(&captured),
        })));
        let writer = Arc::new(
            PtyInputWriter::spawn(writer, Duration::from_millis(100), PTY_LATE_SUBMIT_BOUND)
                .unwrap(),
        );

        let first_writer = Arc::clone(&writer);
        let first = std::thread::spawn(move || first_writer.write_bounded(b"text"));
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            b"text"
        );
        assert!(matches!(
            first.join().unwrap(),
            Err(AgentError::WriteTimeout { .. })
        ));

        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while writer.is_busy() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(*captured.lock().unwrap(), b"text");

        let second_writer = Arc::clone(&writer);
        let second = std::thread::spawn(move || {
            second_writer.write_to_completion_while(b"human key", &|| false)
        });
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            b"human key"
        );
        assert_eq!(
            writer.chain_after_stalled(b"\r", Duration::ZERO),
            ChainOutcome::Landed
        );

        release_tx.send(()).unwrap();
        second.join().unwrap().unwrap();
        assert_eq!(*captured.lock().unwrap(), b"texthuman key");
    }

    #[test]
    fn late_stalled_write_abandons_return_and_reports_distinct_error() {
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let (finished_tx, finished_rx) = channel();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(StalledWrite {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(release_rx),
            finished: finished_tx,
            first: AtomicBool::new(false),
            captured: Arc::clone(&captured),
        })));
        let late_submit_bound = Duration::from_millis(1500);
        let writer = Arc::new(
            PtyInputWriter::spawn(writer, Duration::from_millis(50), late_submit_bound).unwrap(),
        );
        let first_writer = Arc::clone(&writer);
        let first = std::thread::spawn(move || first_writer.write_bounded(b"text"));
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            first.join().unwrap(),
            Err(AgentError::WriteTimeout { .. })
        ));
        std::thread::sleep(late_submit_bound + Duration::from_millis(100));
        assert_eq!(
            writer.chain_after_stalled(b"\r", Duration::ZERO),
            ChainOutcome::Unsupported
        );
        assert!(matches!(
            writer.write_bounded(b"second sender"),
            Err(AgentError::LateSubmitAbandoned { bound }) if bound == late_submit_bound
        ));
        assert!(matches!(
            writer.write_to_completion_while(b"attach key", &|| false),
            Err(AgentError::Busy)
        ));

        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while writer.is_busy() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(*captured.lock().unwrap(), b"text");
        assert!(!writer.is_busy());
        assert!(matches!(
            writer.write_bounded(b"first after late write"),
            Err(AgentError::LateSubmitAbandoned { bound }) if bound == late_submit_bound
        ));
        assert_eq!(*captured.lock().unwrap(), b"text");
        writer.write_bounded(b"second after late write").unwrap();
        assert_eq!(*captured.lock().unwrap(), b"textsecond after late write");
    }

    #[test]
    fn worker_panic_resets_busy_instead_of_sticking_the_session() {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(PanickingWrite)));
        let writer =
            PtyInputWriter::spawn(writer, Duration::from_millis(100), PTY_LATE_SUBMIT_BOUND)
                .unwrap();

        assert!(matches!(
            writer.write_bounded(b"panic"),
            Err(AgentError::Io(_))
        ));
        assert!(!writer.is_busy());
        assert!(matches!(
            writer.write_bounded(b"still available"),
            Err(AgentError::Io(_))
        ));
        assert!(!writer.is_busy());
    }

    #[test]
    fn cursor_query_does_not_block_behind_a_stalled_input_write() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter =
            Arc::new(Mutex::new(Box::new(CaptureWrites(Arc::clone(&captured)))));
        let guard = writer.lock().unwrap();
        let started = std::time::Instant::now();
        answer_cursor_query(&writer, (1, 2));
        assert!(started.elapsed() < Duration::from_millis(100));
        drop(guard);
        assert!(captured.lock().unwrap().is_empty());
    }
}

impl PtyAgent {
    fn record_exit_status(&mut self, status: portable_pty::ExitStatus) {
        let signal = status.signal().and_then(signal_number);
        self.exit_info = Some(ExitInfo {
            exit_code: if signal.is_none() {
                Some(status.exit_code())
            } else {
                None
            },
            signal,
            signal_name: signal_name(signal.unwrap_or_default()),
        });
    }
}

fn signal_number(description: &str) -> Option<i32> {
    if let Some(number) = description
        .rsplit_once(':')
        .and_then(|(_, number)| number.trim().parse().ok())
    {
        return Some(number);
    }

    #[cfg(unix)]
    {
        let description = description
            .split_once(':')
            .map_or(description, |(name, _)| name)
            .trim()
            .to_ascii_lowercase();
        match description.as_str() {
            "hangup" | "hangup (terminal line hangup)" => Some(libc::SIGHUP),
            "interrupt" | "interrupt (user)" => Some(libc::SIGINT),
            "quit" | "quit (core dumped)" => Some(libc::SIGQUIT),
            "illegal instruction" | "illegal instruction (core dumped)" => Some(libc::SIGILL),
            "trace/bpt trap" | "trace/breakpoint trap" => Some(libc::SIGTRAP),
            "abort trap" | "aborted" | "abort trap (core dumped)" => Some(libc::SIGABRT),
            "bus error" | "bus error (core dumped)" => Some(libc::SIGBUS),
            "floating point exception" | "arithmetic exception" => Some(libc::SIGFPE),
            "killed" | "killed (no core)" => Some(libc::SIGKILL),
            "user defined signal 1" => Some(libc::SIGUSR1),
            "user defined signal 2" => Some(libc::SIGUSR2),
            "segmentation fault" | "segmentation fault (core dumped)" => Some(libc::SIGSEGV),
            "broken pipe" => Some(libc::SIGPIPE),
            "alarm clock" => Some(libc::SIGALRM),
            "terminated" | "termination" => Some(libc::SIGTERM),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = description;
        None
    }
}

#[cfg(unix)]
fn signal_name(signal: i32) -> Option<String> {
    let name = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGTERM => "SIGTERM",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        _ => return None,
    };
    Some(name.to_string())
}

#[cfg(not(unix))]
fn signal_name(_: i32) -> Option<String> {
    None
}

fn capture_snapshot(
    screen: &Arc<Mutex<vt100::Parser>>,
    size: Size,
    scrollback_total: &AtomicUsize,
    output_version: &AtomicU64,
    scrollback: usize,
) -> Result<VersionedSnapshot> {
    let mut parser = screen.lock().map_err(|_| io("screen lock poisoned"))?;
    let screen = parser.screen_mut();
    let previous = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let scrollback_len = screen.scrollback();
    screen.set_scrollback(previous);
    let (cells, wrapped, cursor) = with_scrollback(screen, scrollback, |screen| {
        let cursor = if scrollback == 0 {
            let (row, col) = display_cursor(screen);
            Cursor {
                row,
                col,
                visible: !screen.hide_cursor(),
            }
        } else {
            Cursor {
                row: 0,
                col: 0,
                visible: false,
            }
        };
        (
            styled_cells(screen, size),
            (0..size.rows())
                .map(|row| screen.row_wrapped(row))
                .collect(),
            cursor,
        )
    });
    let version = output_version.load(Ordering::SeqCst);
    Ok(VersionedSnapshot {
        snapshot: ScreenSnapshot {
            cells,
            wrapped,
            cursor,
            scrollback_len,
            scrollback_total: scrollback_total.load(Ordering::Relaxed),
        },
        output_version: Some(version),
        instance_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[cfg(unix)]
    #[test]
    fn portable_pty_signal_descriptions_map_without_numeric_suffixes() {
        assert_eq!(signal_number("Terminated"), Some(libc::SIGTERM));
        assert_eq!(signal_number("Killed"), Some(libc::SIGKILL));
        assert_eq!(signal_number("Hangup"), Some(libc::SIGHUP));
        assert_eq!(signal_number("Interrupt"), Some(libc::SIGINT));
        assert_eq!(signal_number("Terminated: 15"), Some(libc::SIGTERM));
    }

    #[test]
    fn temporary_scrollback_view_restores_after_panic() {
        let mut parser = vt100::Parser::new(2, 5, 4);
        parser.process(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        parser.screen_mut().set_scrollback(1);
        let original = parser.screen().scrollback();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_scrollback(parser.screen_mut(), 0, |_| panic!("exercise unwind"));
        }));

        assert!(panic.is_err());
        assert_eq!(parser.screen().scrollback(), original);
    }

    #[test]
    fn monotonic_scrollback_counter_advances_after_retained_history_is_full() {
        let mut parser = vt100::Parser::new(24, 80, SCROLLBACK_ROWS);
        let total = AtomicUsize::new(0);
        let fill: String = (0..SCROLLBACK_ROWS + 100)
            .map(|row| format!("row-{row:05}\r\n"))
            .collect();
        process_output(&mut parser, fill.as_bytes(), &total);
        assert_eq!(retained_history_len(&mut parser), SCROLLBACK_ROWS);
        assert!(total.load(Ordering::Relaxed) >= SCROLLBACK_ROWS);

        let before = total.load(Ordering::Relaxed);
        process_output(&mut parser, b"tail\r\ntail\r\ntail\r\n", &total);
        assert_eq!(retained_history_len(&mut parser), SCROLLBACK_ROWS);
        assert_eq!(total.load(Ordering::Relaxed) - before, 3);
    }

    #[test]
    fn scrollback_counter_is_exact_when_crlf_is_split_across_output_chunks() {
        let mut parser = vt100::Parser::new(2, 10, 20);
        let total = AtomicUsize::new(0);
        for part in [b"a\r".as_slice(), b"\n", b"b\r", b"\n", b"c\r", b"\n"] {
            process_output(&mut parser, part, &total);
        }
        assert_eq!(total.load(Ordering::Relaxed), 2);
        assert_eq!(retained_history_len(&mut parser), 2);
    }

    fn retained_history_len(parser: &mut vt100::Parser) -> usize {
        let screen = parser.screen_mut();
        let previous = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let len = screen.scrollback();
        screen.set_scrollback(previous);
        len
    }

    fn wait_for(agent: &mut PtyAgent, needle: &str) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if agent.screen_text().unwrap().contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// [MEASURED, Linux] Reproduces "no colour in the TUI pane": `screen_text`
    /// (what `Capture` serves) discards SGR that `screen_bytes` (what `attach`
    /// uses) keeps. See steps/018.
    #[test]
    fn screen_text_strips_colour_that_screen_bytes_keeps() {
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("\x1b[31mred\x1b[0m");
        let mut agent = PtyAgent::spawn(cmd, Size::new(80, 24)).unwrap();
        wait_for(&mut agent, "red");

        let text = agent.screen_text().unwrap();
        let bytes = agent.screen_bytes().unwrap();

        assert!(
            !text.contains('\x1b'),
            "screen_text (Capture, what the TUI pane reads) must be plain: {text:?}"
        );
        assert!(
            bytes.windows(2).any(|w| w == b"\x1b["),
            "screen_bytes (attach's initial repaint) must carry SGR: {bytes:?}"
        );
    }

    /// [MEASURED, Linux] `screen_cells` reads `vt100`'s own wide/continuation
    /// judgment for real Hangul, what `PtyAgent` actually hands the pane.
    /// See steps/023.
    #[test]
    fn screen_cells_marks_a_wide_hangul_cell_and_its_empty_continuation() {
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("안녕!");
        let mut agent = PtyAgent::spawn(cmd, Size::new(80, 24)).unwrap();
        wait_for(&mut agent, "안녕!");

        let cells = agent.screen_cells().unwrap();
        let row0 = &cells[0];
        assert_eq!(row0[0].text, "안");
        assert!(row0[0].wide, "안 is East-Asian Wide: {:?}", row0[0]);
        assert_eq!(
            row0[1].text, "",
            "a wide cell's continuation carries no text of its own: {:?}",
            row0[1]
        );
        assert!(!row0[1].wide);
        assert_eq!(row0[2].text, "녕");
        assert!(row0[2].wide);
        assert_eq!(row0[3].text, "");
        assert!(!row0[3].wide);
        assert_eq!(row0[4].text, "!");
        assert!(!row0[4].wide);
        assert_eq!(
            row0[5].text, " ",
            "a genuinely blank ordinary cell still claims 1 column: {:?}",
            row0[5]
        );
    }

    /// [MEASURED, Linux] `screen_cells` is the styled readout `screen_text`
    /// cannot be: the cells under "red" carry the colour, and a cell the
    /// child never touched stays `Color::Default`. See steps/020.
    #[test]
    fn screen_cells_carries_colour_that_screen_text_discards() {
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("\x1b[31mred\x1b[0m");
        let mut agent = PtyAgent::spawn(cmd, Size::new(80, 24)).unwrap();
        wait_for(&mut agent, "red");

        let cells = agent.screen_cells().unwrap();
        let row0 = &cells[0];
        assert_eq!(row0[0].text, "r");
        assert_eq!(row0[1].text, "e");
        assert_eq!(row0[2].text, "d");
        for c in &row0[0..3] {
            assert_eq!(c.fg, Color::Idx(1), "SGR 31 (red) is vt100 Idx(1): {c:?}");
        }
        assert_eq!(
            row0[3].fg,
            Color::Default,
            "a cell after the \\x1b[0m reset, never itself painted, stays default: {:?}",
            row0[3]
        );
    }

    /// [MEASURED, Linux] `vt100::Screen::hide_cursor` is `mode(MODE_HIDE_CURSOR)`
    /// under the hood — it tracks the child's own DECTCEM request rather than
    /// this process inferring it from raw bytes. See steps/027.
    #[test]
    fn cursor_reports_the_childs_own_hide_request() {
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("before\x1b[?25lhidden");
        let mut agent = PtyAgent::spawn(cmd, Size::new(80, 24)).unwrap();
        wait_for(&mut agent, "hidden");

        let cursor = agent.cursor().unwrap();
        assert!(
            !cursor.visible,
            "the child asked for \\x1b[?25l — the pane must not paint a caret: {cursor:?}"
        );
    }
}
