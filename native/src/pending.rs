//! Bounded completion channels for deferred extension-command replies.

use crate::reply_limit::MAX_REPLY_BYTES;
use mlua::{LuaString, UserData, UserDataMethods};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_PENDING_REPLIES: usize = 64;
pub const PENDING_MARKER_PREFIX: &str = "\u{1e}REMUDA_PENDING:";
const CLIENT_POLL: Duration = Duration::from_millis(25);

#[derive(Debug)]
pub struct CommandResult {
    pub exit_code: u8,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum Completion {
    Result(CommandResult),
    Failure(String),
}

#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub id: u64,
    pub reason: Option<String>,
}

struct State {
    status: AtomicU8, // 0 pending, 1 completed, 2 cancelled
    cancel_reason: Mutex<Option<&'static str>>,
    late_logged: AtomicU8,
}

impl State {
    fn new() -> Self {
        Self {
            status: AtomicU8::new(0),
            cancel_reason: Mutex::new(None),
            late_logged: AtomicU8::new(0),
        }
    }

    fn cancel(&self, reason: &'static str) -> bool {
        let mut cancel_reason = self.cancel_reason.lock().unwrap_or_else(|p| p.into_inner());
        if self
            .status
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            *cancel_reason = Some(reason);
            true
        } else {
            false
        }
    }
}

struct Entry {
    id: u64,
    timeout: Duration,
    state: Arc<State>,
    _result_tx: Sender<Completion>,
    result_rx: Mutex<Option<Receiver<Completion>>>,
    signal_tx: Sender<Signal>,
    signal_rx: Mutex<Option<Receiver<Signal>>>,
    event_tx: Sender<PendingEvent>,
}

enum Signal {
    Shutdown(Sender<()>),
}

struct Inner {
    entries: Mutex<HashMap<u64, Arc<Entry>>>,
    created: Mutex<Vec<u64>>,
    next_id: std::sync::atomic::AtomicU64,
    events_tx: Sender<PendingEvent>,
    events_rx: Mutex<Receiver<PendingEvent>>,
    stopping: AtomicU8,
    marker_token: String,
}

#[derive(Clone)]
pub struct PendingReplies(Arc<Inner>);

impl Default for PendingReplies {
    fn default() -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        Self(Arc::new(Inner {
            entries: Mutex::new(HashMap::new()),
            created: Mutex::new(Vec::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
            events_tx,
            events_rx: Mutex::new(events_rx),
            stopping: AtomicU8::new(0),
            marker_token: format!(
                "{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    ^ ((std::process::id() as u128) << 64)
            ),
        }))
    }
}

pub struct WaitResult {
    pub completion: Result<Completion, String>,
    pub shutdown_ack: Option<Sender<()>>,
}

impl PendingReplies {
    pub fn begin_eval(&self) {
        self.0
            .created
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    pub fn pending_id(&self, value: &str) -> Option<u64> {
        let marker = value.strip_prefix(PENDING_MARKER_PREFIX)?;
        let (token, id) = marker.split_once(':')?;
        if token != self.0.marker_token {
            return None;
        }
        let id = id.parse::<u64>().ok()?;
        self.0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&id)
            .then_some(id)
    }

    pub fn finish_eval(&self, keep: Option<u64>) {
        let ids = std::mem::take(&mut *self.0.created.lock().unwrap_or_else(|p| p.into_inner()));
        let mut entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        for id in ids {
            if Some(id) != keep {
                if let Some(entry) = entries.remove(&id) {
                    if entry.state.cancel("abandoned") {
                        let _ = entry.event_tx.send(PendingEvent { id, reason: None });
                    }
                }
            }
        }
    }

    pub fn create(&self, timeout: Duration) -> Result<(u64, PendingHandle), String> {
        if self.0.stopping.load(Ordering::SeqCst) != 0 {
            return Err("daemon is stopping; cannot create a pending reply".into());
        }
        let mut entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        if entries.len() >= MAX_PENDING_REPLIES {
            return Err(format!(
                "too many pending replies (limit {MAX_PENDING_REPLIES} per daemon)"
            ));
        }
        let id = self.0.next_id.fetch_add(1, Ordering::SeqCst);
        let (result_tx, result_rx) = mpsc::channel();
        let (signal_tx, signal_rx) = mpsc::channel();
        let state = Arc::new(State::new());
        let entry = Arc::new(Entry {
            id,
            timeout,
            state: Arc::clone(&state),
            _result_tx: result_tx.clone(),
            result_rx: Mutex::new(Some(result_rx)),
            signal_tx,
            signal_rx: Mutex::new(Some(signal_rx)),
            event_tx: self.0.events_tx.clone(),
        });
        entries.insert(id, entry);
        self.0
            .created
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(id);
        Ok((
            id,
            PendingHandle {
                id,
                result_tx,
                state,
                event_tx: self.0.events_tx.clone(),
                marker: format!(
                    "{}{token}:{id}",
                    PENDING_MARKER_PREFIX,
                    token = self.0.marker_token
                ),
            },
        ))
    }

    pub fn wait(
        &self,
        id: u64,
        mut client_disconnected: impl FnMut() -> bool,
    ) -> Result<WaitResult, String> {
        let entry = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
            .cloned()
            .ok_or_else(|| "deferred reply is no longer available".to_string())?;
        let result_rx = entry
            .result_rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| "deferred reply already has a waiter".to_string())?;
        let signal_rx = entry
            .signal_rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| "deferred reply already has a waiter".to_string())?;
        let deadline = Instant::now() + entry.timeout;
        let (completion, shutdown_ack) = loop {
            match result_rx.try_recv() {
                Ok(completion) => break (Ok(completion), None),
                Err(TryRecvError::Disconnected) => {
                    break (Err("deferred reply was abandoned".to_string()), None)
                }
                Err(TryRecvError::Empty) => {}
            }
            if let Ok(Signal::Shutdown(ack)) = signal_rx.try_recv() {
                if cancel_entry(&entry, "shutdown") {
                    break (Err("daemon stopping".into()), Some(ack));
                }
                if entry.state.status.load(Ordering::SeqCst) == 1 {
                    let completion = result_rx
                        .recv()
                        .map_err(|_| "deferred reply was abandoned".to_string());
                    break (completion, Some(ack));
                }
                break (Err("daemon stopping".into()), Some(ack));
            }
            if client_disconnected() {
                if cancel_entry(&entry, "client_disconnected") {
                    break (Err("client disconnected".into()), None);
                }
                if entry.state.status.load(Ordering::SeqCst) == 1 {
                    break (
                        result_rx
                            .recv()
                            .map_err(|_| "deferred reply was abandoned".to_string()),
                        None,
                    );
                }
                break (Err("client disconnected".into()), None);
            }
            let now = Instant::now();
            if now >= deadline {
                if cancel_entry(&entry, "timeout") {
                    break (Err("deferred command timed out".into()), None);
                }
                if entry.state.status.load(Ordering::SeqCst) == 1 {
                    break (
                        result_rx
                            .recv()
                            .map_err(|_| "deferred reply was abandoned".to_string()),
                        None,
                    );
                }
                break (Err("deferred command timed out".into()), None);
            }
            let wait = (deadline - now).min(CLIENT_POLL);
            match result_rx.recv_timeout(wait) {
                Ok(completion) => break (Ok(completion), None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break (Err("deferred reply was abandoned".to_string()), None)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            }
        };
        self.0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        Ok(WaitResult {
            completion,
            shutdown_ack,
        })
    }

    pub fn drain_events(&self) -> Vec<PendingEvent> {
        self.0
            .events_rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_iter()
            .collect()
    }

    pub fn shutdown(&self) {
        if self.0.stopping.swap(1, Ordering::SeqCst) != 0 {
            return;
        }
        let entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        let (ack_tx, ack_rx) = mpsc::channel();
        let mut expected = 0;
        for entry in entries.values() {
            if cancel_entry(entry, "shutdown")
                && entry
                    .signal_tx
                    .send(Signal::Shutdown(ack_tx.clone()))
                    .is_ok()
            {
                expected += 1;
            }
        }
        drop(ack_tx);
        drop(entries);
        let deadline = Instant::now() + Duration::from_secs(1);
        for _ in 0..expected {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || ack_rx.recv_timeout(remaining).is_err() {
                break;
            }
        }
    }

    pub fn abandon(&self, id: u64) {
        if let Some(entry) = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id)
        {
            if entry.state.cancel("client_disconnected") {
                let _ = entry.event_tx.send(PendingEvent {
                    id,
                    reason: Some("client_disconnected".into()),
                });
            }
        }
    }
}

fn cancel_entry(entry: &Entry, reason: &'static str) -> bool {
    if entry.state.cancel(reason) {
        let _ = entry.event_tx.send(PendingEvent {
            id: entry.id,
            reason: Some(reason.to_string()),
        });
        true
    } else {
        false
    }
}

pub struct PendingHandle {
    id: u64,
    result_tx: Sender<Completion>,
    state: Arc<State>,
    event_tx: Sender<PendingEvent>,
    marker: String,
}

impl PendingHandle {
    pub fn marker(&self) -> &str {
        &self.marker
    }

    fn complete(&self, completion: Completion) -> mlua::Result<()> {
        match self
            .state
            .status
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {
                let _ = self.event_tx.send(PendingEvent {
                    id: self.id,
                    reason: None,
                });
                self.result_tx
                    .send(completion)
                    .map_err(|_| mlua::Error::runtime("pending reply is no longer connected"))
            }
            Err(1) => Err(mlua::Error::runtime(
                "pending reply already resolved or rejected",
            )),
            Err(_) => {
                let reason = *self
                    .state
                    .cancel_reason
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if reason == Some("timeout")
                    && self.state.late_logged.swap(1, Ordering::SeqCst) == 0
                {
                    eprintln!("remuda: ignored late completion of timed-out pending reply");
                }
                Ok(())
            }
        }
    }
}

impl UserData for PendingHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "resolve",
            |_, this, (code, stdout, stderr): (i64, LuaString, LuaString)| {
                if !(0..=255).contains(&code) {
                    return Err(mlua::Error::runtime(
                        "pending exit code must be an integer from 0 through 255",
                    ));
                }
                let stdout_bytes = stdout.as_bytes();
                let stderr_bytes = stderr.as_bytes();
                let output_size = stdout_bytes.len().saturating_add(stderr_bytes.len());
                if output_size > MAX_REPLY_BYTES {
                    let message = format!(
                        "deferred command reply exceeds the {} MiB output limit ({output_size} bytes)",
                        MAX_REPLY_BYTES / (1024 * 1024)
                    );
                    this.complete(Completion::Failure(message.clone()))?;
                    return Err(mlua::Error::runtime(message));
                }
                let stdout = stdout_bytes.to_vec();
                let stderr = stderr_bytes.to_vec();
                this.complete(Completion::Result(CommandResult {
                    exit_code: code as u8,
                    stdout,
                    stderr,
                }))
            },
        );
        methods.add_method("reject", |_, this, error: String| {
            let failure = if error.len() > MAX_REPLY_BYTES {
                format!(
                    "deferred command reply exceeds the {} MiB output limit ({} bytes)",
                    MAX_REPLY_BYTES / (1024 * 1024),
                    error.len(),
                )
            } else {
                error
            };
            this.complete(Completion::Failure(failure))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiter_does_not_report_timeout_after_completion_claim() {
        let pending = PendingReplies::default();
        let (id, handle) = pending.create(Duration::from_millis(20)).unwrap();
        let entry = pending.0.entries.lock().unwrap().get(&id).unwrap().clone();
        // This is the state immediately after complete() wins its CAS and
        // before it publishes the completion into the result channel.
        entry.state.status.store(1, Ordering::SeqCst);
        let waiter = pending.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = waiter.wait(id, || false).unwrap();
            done_tx.send(result.completion).unwrap();
        });

        std::thread::sleep(Duration::from_millis(60));
        assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        handle
            .result_tx
            .send(Completion::Failure("claimed completion".into()))
            .unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(Completion::Failure(message)) if message == "claimed completion"
        ));
        thread.join().unwrap();
    }
}
