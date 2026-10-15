//! Isolated Unix host backend. Windows backend is a separate required gate.
use super::terminal::Terminal;
use crate::pty::{PtyInputWriter, SharedWriter, PTY_WRITE_TIMEOUT};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use remuda_core::agent::{
    AgentError, AgentProcess, AgentWriter, Cursor, ExitInfo, OutputSignal, OutputWakeup, Result,
    Size, StyledCell, VersionedSnapshot,
};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) struct HostChild {
    terminal: Arc<Mutex<Terminal>>,
    writer: Arc<PtyInputWriter>,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    size: Size,
    version: Arc<AtomicU64>,
    wakes: Arc<Mutex<Vec<OutputSignal>>>,
    eof: Arc<AtomicBool>,
    fault: Arc<AtomicBool>,
    exit: Option<ExitInfo>,
}
impl HostChild {
    pub fn spawn(command: CommandBuilder, size: Size) -> Result<Self> {
        let terminal = Arc::new(Mutex::new(
            Terminal::new(size.rows(), size.cols(), 1000).map_err(io)?,
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
        let mut reader = pair.master.try_clone_reader().map_err(io)?;
        let child = pair.slave.spawn_command(command).map_err(io)?;
        drop(pair.slave);
        let context = Reader {
            terminal: Arc::clone(&terminal),
            version: Arc::clone(&version),
            wakes: Arc::clone(&wakes),
            eof: Arc::clone(&eof),
            fault: Arc::clone(&fault),
        };
        std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => break,
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
                        if error.raw_os_error() != Some(libc::EIO) {
                            context.fault.store(true, Ordering::Release);
                        }
                        break;
                    }
                }
            }
            context.eof.store(true, Ordering::Release);
            context.wakes.lock().unwrap().clear();
        });
        Ok(Self {
            terminal,
            writer: input,
            child,
            master: pair.master,
            size,
            version,
            wakes,
            eof,
            fault,
            exit: None,
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
    fault: Arc<AtomicBool>,
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
        self.child.process_id()
    }
    fn subscribe_output_wakeup(&mut self) -> Option<OutputWakeup> {
        let mut wakes = self.wakes.lock().ok()?;
        if wakes.len() >= 16 {
            return None;
        }
        let (signal, wake) = OutputWakeup::pair(Arc::clone(&self.version));
        if !self.output_eof() {
            wakes.push(signal);
        }
        Some(wake)
    }
    fn is_alive(&mut self) -> bool {
        if self.exit.is_some() {
            return false;
        }
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.exit = Some(ExitInfo {
                    exit_code: Some(status.exit_code()),
                    ..ExitInfo::default()
                });
                false
            }
            Err(_) => {
                self.fault.store(true, Ordering::Release);
                true
            } // unknown is not exit zero
        }
    }
    fn exit_info(&mut self) -> Option<ExitInfo> {
        self.exit.clone()
    }
    fn terminate(&mut self) -> Result<()> {
        if self.is_alive() {
            self.child.kill().map_err(io)?;
        }
        // Reaping is polled separately; never join a descendant-held PTY here.
        Ok(())
    }
    fn resize(&mut self, size: Size) -> Result<()> {
        // Validate before mutating either PTY or parser.
        Terminal::validate_geometry(size.rows(), size.cols(), 1000).map_err(io)?;
        let mut terminal = self.terminal.lock().map_err(io)?;
        self.master.resize(pty_size(size)).map_err(io)?;
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
