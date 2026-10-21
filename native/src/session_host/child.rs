//! Isolated Unix host backend. Windows backend is a separate required gate.
use super::terminal::Terminal;
use crate::pty::{PtyInputWriter, SharedWriter, PTY_WRITE_TIMEOUT};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use remuda_core::agent::{
    AgentError, AgentProcess, AgentWriter, Cursor, ExitInfo, MouseEncoding, MouseMode, MouseState,
    OutputSignal, OutputWakeup, Result, Size, StyledCell, VersionedSnapshot,
};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) struct HostChild {
    terminal: Arc<Mutex<Terminal>>,
    writer: Arc<PtyInputWriter>,
    reaper: super::reaper::Reaper,
    master: Box<dyn MasterPty + Send>,
    size: Size,
    version: Arc<AtomicU64>,
    wakes: Arc<Mutex<Vec<OutputSignal>>>,
    eof: Arc<AtomicBool>,
    drained: Arc<AtomicU64>,
    end: Arc<AtomicU8>,
    fault: Arc<AtomicBool>,
}
#[derive(Clone)]
pub(crate) struct View {
    version: Arc<AtomicU64>,
    wakes: Arc<Mutex<Vec<OutputSignal>>>,
    terminal: Arc<Mutex<Terminal>>,
    eof: Arc<AtomicBool>,
    drained: Arc<AtomicU64>,
    end: Arc<AtomicU8>,
    fault: Arc<AtomicBool>,
}
impl View {
    pub fn viewport(&self, offset: usize) -> Result<super::viewport::Viewport> {
        Ok(self.terminal.lock().map_err(io)?.viewport(offset))
    }
    pub fn subscribe(&self) -> Result<OutputWakeup> {
        let mut wakes = self.wakes.lock().map_err(io)?;
        wakes.retain(OutputSignal::is_connected);
        // One core idle observer plus the at-most-sixteen host connections.
        if wakes.len() >= 17 {
            return Err(io("host wake subscription limit"));
        }
        let (signal, wake) = OutputWakeup::pair(Arc::clone(&self.version));
        if !self.eof.load(Ordering::Acquire) {
            wakes.push(signal);
        }
        Ok(wake)
    }
    pub fn status(&self) -> Result<(u64, (u16, u16))> {
        let terminal = self.terminal.lock().map_err(io)?;
        Ok((terminal.generation(), terminal.screen().size()))
    }
    pub fn capture(&self) -> Result<(u64, Vec<u8>, (u16, u16))> {
        let terminal = self.terminal.lock().map_err(io)?;
        Ok((
            terminal.generation(),
            terminal.screen().state_formatted(),
            terminal.screen().size(),
        ))
    }
    pub fn output_end(&self) -> super::api::OutputEnd {
        if !self.eof.load(Ordering::Acquire) {
            return super::api::OutputEnd::Open;
        }
        match self.end.load(Ordering::Acquire) {
            1 => super::api::OutputEnd::Eof,
            2 => super::api::OutputEnd::Hangup,
            _ => super::api::OutputEnd::ReadFailure,
        }
    }
    pub fn drained_generation(&self) -> Option<u64> {
        self.eof
            .load(Ordering::Acquire)
            .then(|| self.drained.load(Ordering::Acquire))
    }
    pub fn health(&self) -> Result<((u64, u64), bool, bool)> {
        Ok((
            self.terminal.lock().map_err(io)?.health(),
            self.eof.load(Ordering::Acquire),
            self.fault.load(Ordering::Acquire),
        ))
    }
}
impl HostChild {
    pub fn view(&self) -> View {
        View {
            version: self.version.clone(),
            wakes: self.wakes.clone(),
            terminal: self.terminal.clone(),
            eof: self.eof.clone(),
            drained: self.drained.clone(),
            end: self.end.clone(),
            fault: self.fault.clone(),
        }
    }
    pub fn writer(&self) -> Arc<dyn AgentWriter> {
        self.writer.clone()
    }

    pub fn spawn(command: CommandBuilder, size: Size) -> Result<Self> {
        let terminal = Arc::new(Mutex::new(
            Terminal::new(size.rows(), size.cols(), super::terminal::HISTORY_ROWS).map_err(io)?,
        ));
        let pair = native_pty_system().openpty(pty_size(size)).map_err(io)?;
        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer().map_err(io)?));
        let input = Arc::new(
            PtyInputWriter::spawn(
                Arc::clone(&writer),
                PTY_WRITE_TIMEOUT,
                Duration::from_secs(30),
            )
            .map_err(io)?,
        );
        let version = Arc::new(AtomicU64::new(0));
        let wakes = Arc::new(Mutex::new(Vec::new()));
        let eof = Arc::new(AtomicBool::new(false));
        let drained = Arc::new(AtomicU64::new(0));
        let end = Arc::new(AtomicU8::new(0));
        let fault = Arc::new(AtomicBool::new(false));
        let (queries, replies) = sync_channel::<(u16, u16)>(32);
        let reply_fault = Arc::clone(&fault);
        std::thread::spawn(move || {
            while let Ok((row, col)) = replies.recv() {
                let reply = format!("\x1b[{row};{col}R");
                let result = writer.lock().map_err(|_| ()).and_then(|mut writer| {
                    writer
                        .write_all(reply.as_bytes())
                        .and_then(|()| writer.flush())
                        .map_err(|_| ())
                });
                if result.is_err() {
                    reply_fault.store(true, Ordering::Release);
                    break;
                }
            }
        });
        let reader = pair.master.try_clone_reader().map_err(io)?;
        let child = pair.slave.spawn_command(command).map_err(io)?;
        drop(pair.slave);
        let reaper = super::reaper::Reaper::start(child, fault.clone());
        let context = Reader {
            terminal: Arc::clone(&terminal),
            version: Arc::clone(&version),
            wakes: Arc::clone(&wakes),
            eof: Arc::clone(&eof),
            drained: Arc::clone(&drained),
            end: Arc::clone(&end),
            fault: Arc::clone(&fault),
        };
        std::thread::spawn(move || drain(reader, context, queries));
        Ok(Self {
            terminal,
            writer: input,
            reaper,
            master: pair.master,
            size,
            version,
            wakes,
            eof,
            drained,
            end,
            fault,
        })
    }
    pub fn output_eof(&self) -> bool {
        self.eof.load(Ordering::Acquire)
    }
    pub fn health(&self) -> ((u64, u64), bool) {
        (
            self.terminal.lock().unwrap().health(),
            self.fault.load(Ordering::Acquire),
        )
    }
}
struct Reader {
    terminal: Arc<Mutex<Terminal>>,
    version: Arc<AtomicU64>,
    wakes: Arc<Mutex<Vec<OutputSignal>>>,
    eof: Arc<AtomicBool>,
    drained: Arc<AtomicU64>,
    end: Arc<AtomicU8>,
    fault: Arc<AtomicBool>,
}
fn drain(
    mut reader: Box<dyn Read + Send>,
    context: Reader,
    queries: std::sync::mpsc::SyncSender<(u16, u16)>,
) {
    let mut bytes = [0; 8192];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => {
                context.end.store(1, Ordering::Release);
                break;
            }
            Ok(count) => {
                let mut remaining = &bytes[..count];
                while !remaining.is_empty() {
                    let mut terminal = context.terminal.lock().unwrap();
                    let Ok(consumed) = terminal.process(remaining) else {
                        context.fault.store(true, Ordering::Release);
                        break;
                    };
                    remaining = &remaining[consumed..];
                    while let Some(query) = terminal.take_query() {
                        if queries.try_send(query).is_err() {
                            context.fault.store(true, Ordering::Release);
                        }
                    }
                    context
                        .version
                        .store(terminal.generation(), Ordering::Release);
                    drop(terminal);
                    context.wakes.lock().unwrap().retain(OutputSignal::wake);
                    std::thread::yield_now();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if error.raw_os_error() == Some(libc::EIO) {
                    context.end.store(2, Ordering::Release);
                } else {
                    context.fault.store(true, Ordering::Release);
                    context.end.store(3, Ordering::Release);
                }
                break;
            }
        }
    }
    context
        .drained
        .store(context.version.load(Ordering::Acquire), Ordering::Release);
    context.eof.store(true, Ordering::Release);
    context.wakes.lock().unwrap().clear();
}
fn io(error: impl std::fmt::Display) -> AgentError {
    AgentError::Io(error.to_string())
}
fn pty_size(size: Size) -> PtySize {
    PtySize {
        rows: size.rows(),
        cols: size.cols(),
        pixel_width: 0,
        pixel_height: 0,
    }
}
impl AgentProcess for HostChild {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_bounded(bytes)
    }
    fn input_writer(&mut self) -> Option<Arc<dyn AgentWriter>> {
        Some(self.writer.clone())
    }
    fn screen_text(&mut self) -> Result<String> {
        Ok(self.terminal.lock().map_err(io)?.screen().contents())
    }
    fn screen_bytes(&mut self) -> Result<Vec<u8>> {
        Ok(self.terminal.lock().map_err(io)?.screen().state_formatted())
    }
    fn screen_cells(&mut self) -> Result<Vec<Vec<StyledCell>>> {
        Ok(self.terminal.lock().map_err(io)?.snapshot(0).snapshot.cells)
    }
    fn screen_snapshot_at(&mut self, scrollback: usize) -> Result<VersionedSnapshot> {
        Ok(self.terminal.lock().map_err(io)?.snapshot(scrollback))
    }
    fn mouse_tracking(&mut self) -> bool {
        self.mouse_state().mode != MouseMode::None
    }
    fn mouse_state(&mut self) -> MouseState {
        let Ok(terminal) = self.terminal.lock() else {
            return MouseState::default();
        };
        let screen = terminal.screen();
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
    }
    fn cursor(&mut self) -> Result<Cursor> {
        let terminal = self.terminal.lock().map_err(io)?;
        let screen = terminal.screen();
        let (row, col) = crate::pty::display_cursor(screen);
        Ok(Cursor {
            row,
            col,
            visible: !screen.hide_cursor(),
        })
    }
    fn output_version(&mut self) -> Option<u64> {
        Some(self.version.load(Ordering::Acquire))
    }
    fn process_id(&self) -> Option<u32> {
        self.reaper.pid
    }
    fn subscribe_output_wakeup(&mut self) -> Option<OutputWakeup> {
        self.view().subscribe().ok()
    }
    fn is_alive(&mut self) -> bool {
        self.reaper.alive()
    }
    fn exit_info(&mut self) -> Option<ExitInfo> {
        self.reaper.info()
    }
    fn terminate(&mut self) -> Result<()> {
        self.reaper.terminate()
    }
    fn resize(&mut self, size: Size) -> Result<()> {
        // Validate before mutating either PTY or parser.
        Terminal::validate_geometry(size.rows(), size.cols(), super::terminal::HISTORY_ROWS)
            .map_err(io)?;
        let mut terminal = self.terminal.lock().map_err(io)?;
        if let Err(error) = terminal.prepare_resize(size.rows(), size.cols()) {
            self.fault.store(true, Ordering::Release);
            return Err(io(error));
        }
        if let Err(error) = self.master.resize(pty_size(size)) {
            self.fault.store(true, Ordering::Release);
            return Err(io(error));
        }
        terminal.resize(size.rows(), size.cols()).map_err(io)?;
        self.version.store(terminal.generation(), Ordering::Release);
        self.size = size;
        self.wakes.lock().map_err(io)?.retain(OutputSignal::wake);
        Ok(())
    }
    fn size(&self) -> Size {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    #[test]
    fn exhausted_generation_refuses_before_os_resize_and_reports_degradation() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            "printf 'resize-ready\\n'; while IFS= read -r line; do :; done",
        ]);
        let mut child = Cleanup(HostChild::spawn(command, Size::new(80, 24)).unwrap());
        wait_for(&mut child.0, "resize-ready");
        child
            .0
            .terminal
            .lock()
            .unwrap()
            .generation_fixture(u64::MAX);
        assert!(child.0.resize(Size::new(90, 25)).is_err());
        let actual = child.0.master.get_size().unwrap();
        assert_eq!((actual.rows, actual.cols), (24, 80));
        assert_eq!(child.0.terminal.lock().unwrap().screen().size(), (24, 80));
        assert!(child.0.fault.load(Ordering::Acquire));
    }
    struct Cleanup(HostChild);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.terminate();
            let until = Instant::now() + Duration::from_secs(2);
            while self.0.is_alive() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    fn wait_for(child: &mut HostChild, expected: &str) {
        let until = Instant::now() + Duration::from_secs(3);
        while !child.screen_text().unwrap().contains(expected) {
            assert!(Instant::now() < until, "fixture output did not arrive");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn isolated_child_computes_input_and_retains_terminal_without_viewers() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.args(["-c", "stty -echo; printf 'fixture-ready\\n'; while IFS= read -r value; do printf 'computed:%s\\n' \"$((value+7))\"; done"]);
        let mut child = Cleanup(HostChild::spawn(command, Size::new(80, 24)).unwrap());
        wait_for(&mut child.0, "fixture-ready");
        let pid = child.0.process_id();
        child.0.write(b"35\n").unwrap();
        wait_for(&mut child.0, "computed:42");
        let generation = child.0.output_version().unwrap();
        assert!(generation > 0);
        child.0.resize(Size::new(90, 25)).unwrap();
        assert!(child.0.output_version().unwrap() > generation);
        // No viewer subscribes at any time; drain/parser are independently owned.
        child.0.write(b"44\n").unwrap();
        wait_for(&mut child.0, "computed:51");
        assert_eq!(child.0.process_id(), pid);
        assert_eq!(child.0.health(), ((0, 0), false));
    }
    #[test]
    fn quiet_subscriber_drop_does_not_exhaust_registration() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.args(["-c", "printf 'quiet-ready\\n'; read ignored"]);
        let mut child = Cleanup(HostChild::spawn(command, Size::new(80, 24)).unwrap());
        wait_for(&mut child.0, "quiet-ready");
        for _ in 0..100 {
            drop(
                child
                    .0
                    .subscribe_output_wakeup()
                    .expect("slot reclaimed without further output"),
            );
        }
        assert!(child.0.wakes.lock().unwrap().len() <= 1);
    }
    #[test]
    fn wait_status_and_final_output_are_separate_and_nonzero_is_retained() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.args(["-c", "printf 'final-computed:%s\\n' \"$((6*7))\"; exit 7"]);
        let mut child = Cleanup(HostChild::spawn(command, Size::new(80, 24)).unwrap());
        wait_for(&mut child.0, "final-computed:42");
        let until = Instant::now() + Duration::from_secs(3);
        while child.0.is_alive() || !child.0.output_eof() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(child.0.exit_info().unwrap().exit_code, Some(7));
        assert!(child.0.screen_text().unwrap().contains("final-computed:42"));
    }
}
