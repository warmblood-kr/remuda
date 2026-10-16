//! Sole child waiter, independent of core Session's alive latch and PTY EOF.
use portable_pty::Child;
use remuda_core::agent::{AgentError, ExitInfo, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
type Exit = Arc<(
    Mutex<Option<std::result::Result<ExitInfo, String>>>,
    Condvar,
)>;
pub(crate) struct Reaper {
    pub pid: Option<u32>,
    stop: SyncSender<()>,
    exit: Exit,
}
impl Reaper {
    pub fn start(mut child: Box<dyn Child + Send + Sync>, fault: Arc<AtomicBool>) -> Self {
        let pid = child.process_id();
        let (stop, commands) = sync_channel(1);
        let exit: Exit = Arc::new((Mutex::new(None), Condvar::new()));
        let result = exit.clone();
        std::thread::spawn(move || {
            let mut stopping = false;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        cache(&result, status);
                        break;
                    }
                    Err(error) => {
                        fault.store(true, Ordering::Release);
                        *result.0.lock().unwrap() = Some(Err(error.to_string()));
                        result.1.notify_all();
                        break;
                    }
                    Ok(None) => {}
                }
                match commands.recv_timeout(Duration::from_millis(5)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) if !stopping => {
                        stopping = true;
                        if let Err(error) = child.kill() {
                            // Retry wait once: natural exit may have raced kill.
                            match child.try_wait() {
                                Ok(Some(status)) => cache(&result, status),
                                _ => {
                                    fault.store(true, Ordering::Release);
                                    *result.0.lock().unwrap() = Some(Err(error.to_string()));
                                    result.1.notify_all();
                                }
                            }
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    _ => {}
                }
            }
        });
        Self { pid, stop, exit }
    }
    pub fn alive(&self) -> bool {
        !matches!(&*self.exit.0.lock().unwrap(), Some(Ok(_)))
    }
    pub fn info(&self) -> Option<ExitInfo> {
        self.exit
            .0
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|value| value.as_ref().ok())
            .cloned()
    }
    pub fn terminate(&self) -> Result<()> {
        let _ = self.stop.try_send(());
        let state = self
            .exit
            .0
            .lock()
            .map_err(|_| AgentError::Io("host wait cache poisoned".into()))?;
        let (state, _) = self
            .exit
            .1
            .wait_timeout_while(state, Duration::from_secs(2), |state| state.is_none())
            .map_err(|_| AgentError::Io("host wait cache poisoned".into()))?;
        match state.as_ref() {
            Some(Ok(_)) => Ok(()),
            Some(Err(error)) => Err(AgentError::Io(format!("host wait unknown: {error}"))),
            None => Err(AgentError::Io(
                "host close pending: direct-child wait not confirmed".into(),
            )),
        }
    }
}

fn cache(result: &Exit, status: portable_pty::ExitStatus) {
    let signal = status.signal().and_then(crate::pty::signal_number);
    *result.0.lock().unwrap() = Some(Ok(ExitInfo {
        exit_code: if status.signal().is_none() {
            Some(status.exit_code())
        } else {
            None
        },
        signal,
        signal_name: status.signal().map(str::to_owned),
    }));
    result.1.notify_all();
}
