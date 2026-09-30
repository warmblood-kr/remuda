//! Bounded completion channels for deferred extension-command replies.

use crate::reply_limit::MAX_REPLY_BYTES;
use mlua::{LuaString, UserData, UserDataMethods};
use remuda_core::protocol::SecretBytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_PENDING_REPLIES: usize = 64;
pub const PENDING_MARKER_PREFIX: &str = "\u{1e}REMUDA_PENDING:";
const CLIENT_POLL: Duration = Duration::from_millis(25);
pub const SECRET_PROMPT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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

#[derive(Debug, Clone)]
pub struct SecretPrompt {
    pub id: u32,
    pub label: String,
    pub caller_session: Option<String>,
}

pub struct SecretPromptEvent {
    pub pending_id: u64,
    pub prompt_id: u32,
    pub answer: Result<Option<SecretBytes>, String>,
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
    prompt_tx: Sender<SecretPrompt>,
    prompt_rx: Mutex<Option<Receiver<SecretPrompt>>>,
    prompt_id: Arc<std::sync::atomic::AtomicU32>,
    prompt_outstanding: Arc<AtomicU8>,
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
    secret_events: Mutex<Vec<SecretPromptEvent>>,
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
            secret_events: Mutex::new(Vec::new()),
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
        let (prompt_tx, prompt_rx) = mpsc::channel();
        let state = Arc::new(State::new());
        let entry = Arc::new(Entry {
            id,
            timeout,
            state: Arc::clone(&state),
            _result_tx: result_tx.clone(),
            result_rx: Mutex::new(Some(result_rx)),
            signal_tx,
            signal_rx: Mutex::new(Some(signal_rx)),
            prompt_tx,
            prompt_rx: Mutex::new(Some(prompt_rx)),
            prompt_id: Arc::new(std::sync::atomic::AtomicU32::new(1)),
            prompt_outstanding: Arc::new(AtomicU8::new(0)),
            event_tx: self.0.events_tx.clone(),
        });
        entries.insert(id, Arc::clone(&entry));
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
                prompt_tx: entry.prompt_tx.clone(),
                prompt_id: Arc::clone(&entry.prompt_id),
                prompt_outstanding: Arc::clone(&entry.prompt_outstanding),
                caller_session: None,
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
        mut prompt_client: impl FnMut(SecretPrompt, Duration) -> Result<Option<SecretBytes>, String>,
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
        let prompt_rx = entry
            .prompt_rx
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
            if let Ok(prompt) = prompt_rx.try_recv() {
                serve_secret_prompt(&self.0, &entry, id, prompt, deadline, &mut prompt_client);
                continue;
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

    pub fn drain_secret_events(&self) -> Vec<SecretPromptEvent> {
        std::mem::take(
            &mut *self
                .0
                .secret_events
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        )
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

fn serve_secret_prompt(
    inner: &Inner,
    entry: &Entry,
    pending_id: u64,
    prompt: SecretPrompt,
    deadline: Instant,
    prompt_client: &mut impl FnMut(SecretPrompt, Duration) -> Result<Option<SecretBytes>, String>,
) {
    let prompt_timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(SECRET_PROMPT_TIMEOUT);
    let answer = if prompt_timeout.is_zero() {
        Err("cancelled".into())
    } else {
        prompt_client(prompt.clone(), prompt_timeout)
    };
    entry.prompt_outstanding.store(0, Ordering::SeqCst);
    inner
        .secret_events
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(SecretPromptEvent {
            pending_id,
            prompt_id: prompt.id,
            answer,
        });
}

pub struct PendingHandle {
    id: u64,
    result_tx: Sender<Completion>,
    state: Arc<State>,
    event_tx: Sender<PendingEvent>,
    prompt_tx: Sender<SecretPrompt>,
    prompt_id: Arc<std::sync::atomic::AtomicU32>,
    prompt_outstanding: Arc<AtomicU8>,
    caller_session: Option<String>,
    marker: String,
}

impl PendingHandle {
    pub fn marker(&self) -> &str {
        &self.marker
    }

    pub fn set_caller_session(&mut self, session: Option<String>) {
        self.caller_session = session;
    }

    fn prompt_secret(&self, label: String) -> mlua::Result<u32> {
        if self.state.status.load(Ordering::SeqCst) != 0 {
            return Err(mlua::Error::runtime("pending reply is no longer active"));
        }
        let prompt_id = self.prompt_id.fetch_add(1, Ordering::SeqCst);
        if prompt_id == 0 {
            return Err(mlua::Error::runtime("secret prompt id space exhausted"));
        }
        if self.prompt_outstanding.swap(1, Ordering::SeqCst) != 0 {
            return Err(mlua::Error::runtime(
                "a secret prompt is already outstanding for this pending reply",
            ));
        }
        self.prompt_tx
            .send(SecretPrompt {
                id: prompt_id,
                label,
                caller_session: self.caller_session.clone(),
            })
            .map_err(|_| {
                self.prompt_outstanding.store(0, Ordering::SeqCst);
                mlua::Error::runtime("pending reply is no longer connected")
            })?;
        Ok(prompt_id)
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
        methods.add_method("prompt_secret", |_, this, label: String| {
            this.prompt_secret(label)
        });
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
            let result = waiter.wait(id, || false, |_, _| Ok(None)).unwrap();
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

    #[test]
    fn pending_timeout_caps_prompt_wait_and_handle_completes_once() {
        let pending = PendingReplies::default();
        let (id, handle) = pending.create(Duration::from_millis(200)).unwrap();
        let prompt_id = handle.prompt_secret("Password".into()).unwrap();
        assert!(handle.prompt_secret("Second".into()).is_err());
        let waiter = pending.clone();
        let result = waiter
            .wait(
                id,
                || false,
                |prompt, timeout| {
                    assert_eq!(prompt.id, prompt_id);
                    assert_eq!(prompt.label, "Password");
                    assert!(timeout <= Duration::from_millis(200));
                    assert!(timeout < SECRET_PROMPT_TIMEOUT);
                    Ok(Some(SecretBytes::new(b"probe".to_vec())))
                },
            )
            .unwrap();
        assert!(
            matches!(result.completion, Err(ref error) if error == "deferred command timed out")
        );
        let events = pending.drain_secret_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].pending_id, id);
        assert_eq!(events[0].prompt_id, prompt_id);
        assert_eq!(
            events[0]
                .answer
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_bytes(),
            b"probe"
        );

        let (id, handle) = pending.create(Duration::from_millis(20)).unwrap();
        handle.complete(Completion::Failure("done".into())).unwrap();
        assert!(handle
            .complete(Completion::Failure("twice".into()))
            .is_err());
        assert!(matches!(
            pending
                .wait(id, || false, |_, _| Ok(None))
                .unwrap()
                .completion,
            Ok(Completion::Failure(message)) if message == "done"
        ));
    }
}
