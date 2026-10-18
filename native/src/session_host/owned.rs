//! One stable Session owns complete logical acts; no daemon-side safety owner.
use super::child::{HostChild, View};
use super::control::{ActId, ConnectionId, Fence};
use super::coordinator::{Admission, Coordinator, Error};
use super::ledger::Lookup;
use super::result::{Completion, Delivery, ErrorCode, Reply, Submission};
use super::wire::{InputAct, Step};
use portable_pty::CommandBuilder;
use remuda_core::agent::{AgentError, AgentProcess, AgentWriter, Size};
use remuda_core::{InputSubmitOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) struct Owned {
    session: Arc<Session>,
    incarnation: [u8; 16],
    pid: Option<u32>,
    pub view: View,
    writer: Arc<dyn AgentWriter>,
    coordinator: Mutex<Coordinator>,
}
impl Owned {
    pub fn spawn(
        incarnation: [u8; 16],
        name: String,
        command: CommandBuilder,
        size: Size,
    ) -> Result<Self, AgentError> {
        Self::spawn_with_clock(
            incarnation,
            name,
            command,
            size,
            Arc::new(crate::SystemClock::new()),
        )
    }
    fn spawn_with_clock(
        incarnation: [u8; 16],
        name: String,
        command: CommandBuilder,
        size: Size,
        clock: Arc<dyn remuda_core::Clock>,
    ) -> Result<Self, AgentError> {
        Self::spawn_started(incarnation, name, command, size, clock, None)
    }
    pub fn spawn_reserved(
        identity: super::api::Identity,
        name: String,
        mut command: CommandBuilder,
        size: Size,
    ) -> Result<Self, AgentError> {
        let start =
            remuda_core::session::StartIdentity::new(identity.session.clone(), identity.instance)?;
        if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
            return Err(AgentError::Io("invalid reserved session name".into()));
        }
        command.env("REMUDA_SESSION_ID", identity.session);
        command.env("REMUDA_SESSION_NAME", &name);
        Self::spawn_started(
            identity.host,
            name,
            command,
            size,
            Arc::new(crate::SystemClock::new()),
            Some(start),
        )
    }
    fn spawn_started(
        incarnation: [u8; 16],
        name: String,
        command: CommandBuilder,
        size: Size,
        clock: Arc<dyn remuda_core::Clock>,
        start: Option<remuda_core::session::StartIdentity>,
    ) -> Result<Self, AgentError> {
        let child = HostChild::spawn(command, size)?;
        let pid = child.process_id();
        let view = child.view();
        let writer = child.writer();
        let session = Arc::new(match start {
            Some(start) => Session::with_start_identity(name, start, Box::new(child), clock),
            None => Session::new(name, Box::new(child), clock),
        });
        Ok(Self {
            session,
            incarnation,
            pid,
            view,
            writer,
            coordinator: Mutex::new(Coordinator::new(incarnation)),
        })
    }
    pub fn identity(&self) -> super::api::Identity {
        super::api::Identity {
            host: self.incarnation,
            session: self.session.id().into(),
            instance: self.session.instance_id().into(),
        }
    }
    pub fn inspect(&self) -> Result<super::api::Info, AgentError> {
        let alive = self.session.is_alive();
        let exit = self.session.exit_info();
        let ((unsupported_osc, query_overflow), output_eof, backend_fault) = self.view.health()?;
        let (generation, (rows, cols)) = self.view.status()?;
        let lifecycle = if backend_fault || (!alive && exit.is_none()) {
            super::api::Lifecycle::Unavailable
        } else if alive {
            super::api::Lifecycle::Running
        } else {
            super::api::Lifecycle::Exited
        };
        Ok(super::api::Info {
            publication: None,
            identity: self.identity(),
            name: self.session.name().into(),
            pid: self.pid,
            lifecycle,
            exit_code: exit.as_ref().and_then(|value| value.exit_code),
            signal: exit.as_ref().and_then(|value| value.signal),
            signal_name: exit.and_then(|value| value.signal_name),
            generation,
            rows,
            cols,
            output_eof,
            output_end: self.view.output_end(),
            drained_generation: self.view.drained_generation(),
            host_build: crate::dist::BUILD_VERSION.into(),
            unsupported_osc,
            query_overflow,
            backend_fault,
            active_connections: 0,
        })
    }
    pub fn resize(&self, fence: Fence, rows: u16, cols: u16) -> Result<(), Error> {
        if rows < 24 || cols < 20 {
            return Err(Error::Invalid("host resize minimum is 20x24"));
        }
        super::terminal::Terminal::validate_geometry(rows, cols, 1000).map_err(Error::Invalid)?;
        let token = self.coordinator.lock().unwrap().begin_mutation(fence)?;
        let result = self.session.resize(Size::for_pane(cols, rows));
        self.coordinator.lock().unwrap().finish_mutation(token)?;
        result.map_err(|_| Error::Invalid("host resize failed; inspect health"))
    }
    /// Latch Stop under the coordinator lock; caller proves settlement first,
    /// outside this lock. A human holder keeps the host alive.
    pub fn latch_stop(&self, fence: Fence) -> Result<(), Error> {
        let mut coordinator = self.coordinator.lock().unwrap();
        if self.session.is_attached() {
            return Err(Error::Invalid("human attached"));
        }
        coordinator.latch_stop(fence)
    }
    pub fn close(&self, fence: Fence) -> Result<(), Error> {
        let token = self.coordinator.lock().unwrap().begin_mutation(fence)?;
        match self.session.terminate() {
            Ok(()) => self.coordinator.lock().unwrap().finish_mutation(token),
            Err(AgentError::Attached) => {
                self.coordinator.lock().unwrap().finish_mutation(token)?;
                Err(Error::Invalid("human attached"))
            }
            // Unknown in-flight close remains fenced; never grant while its
            // old physical effect could still land later.
            Err(_) => Err(Error::Invalid("host close unknown or pending")),
        }
    }
    pub fn acquire(&self, connection: ConnectionId) -> Result<Fence, Error> {
        self.coordinator.lock().unwrap().acquire(connection)
    }
    pub fn release(&self, fence: Fence) -> Result<(), Error> {
        self.coordinator.lock().unwrap().release(fence)
    }
    pub fn admit(
        &self,
        fence: Fence,
        client: [u8; 16],
        seq: u64,
        act: &InputAct,
    ) -> Result<Admission, Error> {
        if self
            .view
            .health()
            .map_err(|_| Error::Invalid("host health unavailable"))?
            .2
        {
            return Err(Error::Invalid("host backend unavailable"));
        }
        self.coordinator
            .lock()
            .unwrap()
            .admit(fence, client, seq, act)
    }
    pub fn reject_unstarted(&self, token: ActId) -> Result<(), Error> {
        self.coordinator.lock().unwrap().reject_unstarted(token)
    }
    pub fn recover(
        &self,
        client: [u8; 16],
        seq: u64,
        act: &InputAct,
    ) -> Result<(Lookup, Option<Completion>), Error> {
        let coordinator = self.coordinator.lock().unwrap();
        let (lookup, result) = coordinator.recover(client, seq, act)?;
        Ok((lookup, result.cloned()))
    }
    /// Invoke ONLY for the act bound to the admitted token, once. Runtime owns
    /// that binding; request-supplied ActId is never an execution capability.
    pub fn execute(&self, token: ActId) -> Result<(), Error> {
        let act = self.coordinator.lock().unwrap().claim(token)?;
        let result = self.run(&act);
        // Session may return Late/timeout while an OS worker/follow-up is live.
        // Keep the reservation: a lost controller cannot admit a new writer.
        while self.writer.is_busy() {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.coordinator.lock().unwrap().complete(token, result)
    }
    fn run(&self, act: &InputAct) -> Completion {
        let result = match act {
            InputAct::Bytes(bytes) => self.session.send(bytes).map(|()| Reply::Unit),
            InputAct::SendLine(text) => self.session.send_line(text).map(|()| Reply::Unit),
            InputAct::InputText(text) => self.session.input_text(text).map(|()| Reply::Unit),
            InputAct::TypeText { text, settle_ms } => self
                .session
                .type_text(text, Duration::from_millis(*settle_ms))
                .map(submission),
            InputAct::Submit(expect) => self.session.submit(expect).map(submission),
            InputAct::Clear(key) => self.session.clear_input(key).map(Reply::Cleared),
            InputAct::Feed(steps) => self
                .session
                .feed(
                    &steps
                        .iter()
                        .map(|step| match step {
                            Step::Bytes(bytes) => remuda_core::protocol::Step::Burst(bytes.clone()),
                            Step::Pause(ms) => remuda_core::protocol::Step::Pause(*ms),
                        })
                        .collect::<Vec<_>>(),
                )
                .map(|()| Reply::Unit),
        };
        match result {
            Ok(reply) => Completion {
                delivery: if matches!(reply, Reply::Submission(Submission::Late)) {
                    Delivery::Indeterminate
                } else {
                    Delivery::Written
                },
                reply,
            },
            // A composite operation can have written a prefix before error;
            // conservatively keep errors indeterminate, never automatic replay.
            Err(error) => Completion {
                delivery: Delivery::Indeterminate,
                reply: Reply::Error(error_code(error)),
            },
        }
    }
}
fn submission(value: InputSubmitOutcome) -> Reply {
    Reply::Submission(match value {
        InputSubmitOutcome::Submitted => Submission::Submitted,
        InputSubmitOutcome::Unverified => Submission::Unverified,
        InputSubmitOutcome::Late => Submission::Late,
    })
}
fn error_code(error: AgentError) -> ErrorCode {
    match error {
        AgentError::Busy => ErrorCode::Busy,
        AgentError::Exited => ErrorCode::Exited,
        AgentError::Attached => ErrorCode::Attached,
        AgentError::HumanInputRecent => ErrorCode::HumanInputRecent,
        AgentError::WriteTimeout { .. } => ErrorCode::WriteTimeout,
        AgentError::SubmitUncertain | AgentError::SubmitPending => ErrorCode::SubmitUncertain,
        AgentError::LateSubmitAbandoned { .. } => ErrorCode::LateSubmitAbandoned,
        AgentError::PauseTooLong { .. } => ErrorCode::Invalid,
        AgentError::Io(_) => ErrorCode::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::super::control::Refusal;
    use super::*;
    use std::time::Instant;
    fn wait_for(host: &Owned, expected: &str) {
        let until = Instant::now() + Duration::from_secs(3);
        while !host.session.screen_text().unwrap().contains(expected) {
            assert!(Instant::now() < until, "synthetic host output timeout");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn reserved_spawn_preserves_exact_core_and_host_identity_and_child_metadata() {
        let identity = super::super::api::Identity {
            host: [4; 16],
            session: "reserved-session".into(),
            instance: "reserved-instance".into(),
        };
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("REMUDA_SESSION_ID", "stale-caller-id");
        command.env("REMUDA_SESSION_NAME", "stale-caller-name");
        command.args([
            "-c",
            "printf '%s:%s\\n' \"$REMUDA_SESSION_ID\" \"$REMUDA_SESSION_NAME\"; read ignored",
        ]);
        let host = Cleanup(Arc::new(
            Owned::spawn_reserved(
                identity.clone(),
                "reserved-name".into(),
                command,
                Size::new(80, 24),
            )
            .unwrap(),
        ));
        wait_for(&host.0, "reserved-session:reserved-name");
        assert_eq!(host.0.identity(), identity);
        assert_eq!(host.0.session.id(), "reserved-session");
        assert_eq!(host.0.session.instance_id(), "reserved-instance");
        assert_eq!(host.0.inspect().unwrap().identity, identity);
        assert_eq!(
            remuda_core::session_operations::SessionOperations::snapshot(
                host.0.session.as_ref(),
                0
            )
            .unwrap()
            .instance_id
            .as_deref(),
            Some("reserved-instance")
        );
    }
    #[test]
    fn invalid_reserved_identity_and_name_refuse_before_process_creation() {
        let identity = super::super::api::Identity {
            host: [4; 16],
            session: "".into(),
            instance: "reserved-instance".into(),
        };
        let error = Owned::spawn_reserved(
            identity.clone(),
            "name".into(),
            CommandBuilder::new("/nonexistent-reserved-fixture"),
            Size::new(80, 24),
        )
        .err()
        .unwrap();
        assert!(
            matches!(error, AgentError::Io(ref message) if message == "invalid preassigned session identity")
        );
        let identity = super::super::api::Identity {
            session: "reserved-session".into(),
            ..identity
        };
        let error = Owned::spawn_reserved(
            identity,
            "invalid\nname".into(),
            CommandBuilder::new("/nonexistent-reserved-fixture"),
            Size::new(80, 24),
        )
        .err()
        .unwrap();
        assert!(
            matches!(error, AgentError::Io(ref message) if message == "invalid reserved session name")
        );
    }
    struct Cleanup(Arc<Owned>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.session.terminate();
            let until = Instant::now() + Duration::from_secs(2);
            while self.0.session.is_alive() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    struct PausingClock {
        real: crate::SystemClock,
        entered: std::sync::mpsc::SyncSender<()>,
        resumed: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl remuda_core::Clock for PausingClock {
        fn now(&self) -> Duration {
            remuda_core::Clock::now(&self.real)
        }
        fn instance_id_seed(&self) -> u128 {
            remuda_core::Clock::instance_id_seed(&self.real)
        }
        fn sleep(&self, duration: Duration) {
            if duration == Duration::from_millis(200) {
                self.entered.send(()).unwrap();
                self.resumed
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            } else {
                remuda_core::Clock::sleep(&self.real, duration);
            }
        }
    }
    #[test]
    fn entire_feed_and_rich_outcome_survive_controller_release_during_pause() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.args(["-c", "stty -echo; count=0; printf 'owned-ready\\n'; while IFS= read -r n; do count=$((count+1)); printf 'effect:%s:%s\\n' \"$count\" \"$((n+7))\"; done"]);
        let (entered, paused) = std::sync::mpsc::sync_channel(1);
        let (resume, resumed) = std::sync::mpsc::sync_channel(1);
        let clock = Arc::new(PausingClock {
            real: crate::SystemClock::new(),
            entered,
            resumed: Mutex::new(resumed),
        });
        let host = Cleanup(Arc::new(
            Owned::spawn_with_clock(
                [9; 16],
                "isolated".into(),
                command,
                Size::new(80, 24),
                clock,
            )
            .unwrap(),
        ));
        wait_for(&host.0, "owned-ready");
        let identity = (
            host.0.session.id().to_owned(),
            host.0.session.instance_id().to_owned(),
            host.0.session.process_id_if_alive(),
        );
        let first = host.0.acquire(ConnectionId(1)).unwrap();
        let act = InputAct::Feed(vec![
            Step::Bytes(b"35".to_vec()),
            Step::Pause(200),
            Step::Bytes(b"\n".to_vec()),
        ]);
        let Admission::Started(token) = host.0.admit(first, [1; 16], 1, &act).unwrap() else {
            panic!("expected fresh act")
        };
        let worker_host = host.0.clone();
        let worker = std::thread::spawn(move || worker_host.execute(token));
        paused.recv_timeout(Duration::from_secs(2)).unwrap();
        host.0.release(first).unwrap();
        assert_eq!(
            host.0.acquire(ConnectionId(2)),
            Err(Error::Control(Refusal::Busy))
        );
        resume.send(()).unwrap();
        worker.join().unwrap().unwrap();
        wait_for(&host.0, "effect:1:42");
        let replacement = host.0.acquire(ConnectionId(2)).unwrap();
        let (status, result) = host.0.recover([1; 16], 1, &act).unwrap();
        assert!(matches!(status, Lookup::Complete(_)));
        assert_eq!(result.unwrap().delivery, Delivery::Written);
        assert!(matches!(
            host.0.admit(replacement, [1; 16], 1, &act).unwrap(),
            Admission::Existing(Lookup::Complete(_))
        ));
        assert!(host.0.execute(token).is_err()); // cannot rerun an old worker
        assert_eq!(
            (
                host.0.session.id().to_owned(),
                host.0.session.instance_id().to_owned(),
                host.0.session.process_id_if_alive()
            ),
            identity
        );
        assert!(!host.0.session.screen_text().unwrap().contains("effect:2:"));
        host.0.release(replacement).unwrap();
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn host_answers_actual_child_dsr_without_a_controller() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.args(["-c", "stty -echo -icanon min 1 time 10; printf '\\033[6n'; value=$(dd bs=1 count=6 2>/dev/null | od -An -tu1); printf 'reply: %s\\nreply-done\\n' \"$value\""]);
        let host = Owned::spawn([6; 16], "dsr-fixture".into(), command, Size::new(80, 24)).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(3);
        let output = loop {
            let text = host.session.screen_text().unwrap();
            if text.contains("reply-done") {
                break text;
            }
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            output
                .split_whitespace()
                .filter_map(|word| word.parse::<u8>().ok())
                .collect::<Vec<_>>(),
            b"\x1b[1;1R"
        );
        host.session.terminate().unwrap();
    }
    #[test]
    fn session_input_text_obeys_host_bracketed_paste_mode() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.args(["-c", "stty -echo -icanon min 1 time 10; printf '\\033[?2004hpaste-ready\\n'; value=$(dd bs=1 count=17 2>/dev/null | od -An -tu1); printf 'paste: %s\\npaste-done\\n' \"$value\""]);
        let host =
            Owned::spawn([5; 16], "paste-fixture".into(), command, Size::new(80, 24)).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while !host.session.screen_text().unwrap().contains("paste-ready") {
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(host.session.mouse_state().bracketed_paste);
        host.session.input_text("probe").unwrap();
        let output = loop {
            let text = host.session.screen_text().unwrap();
            // od wraps 17 bytes onto two lines; the marker means all of them drew.
            if text.contains("paste-done") {
                break text;
            }
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            output
                .split_whitespace()
                .filter_map(|word| word.parse::<u8>().ok())
                .collect::<Vec<_>>(),
            b"\x1b[200~probe\x1b[201~"
        );
        host.session.terminate().unwrap();
    }
    #[test]
    fn session_close_retains_signal_status_and_child_is_actually_reaped() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.args(["-c", "printf 'close-ready\\n'; read ignored"]);
        let host =
            Owned::spawn([7; 16], "close-fixture".into(), command, Size::new(80, 24)).unwrap();
        let pid = host.session.process_id_if_alive().unwrap();
        host.session.terminate().unwrap();
        assert!(!host.session.is_alive());
        let status = host.session.exit_info().unwrap();
        assert_eq!(status.exit_code, None);
        assert!(status.signal.is_some());
        assert!(status.signal_name.is_some());
        let mut ignored = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut ignored, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}

pub(crate) struct Staged(Option<Owned>);
impl Staged {
    pub fn new(host: Owned) -> Self {
        Self(Some(host))
    }
    pub fn host(&self) -> &Owned {
        self.0.as_ref().unwrap()
    }
    pub fn publish(mut self) -> Arc<Owned> {
        Arc::new(self.0.take().unwrap())
    }
}
impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(host) = &self.0 {
            let _ = host.session.terminate();
        }
    }
}
