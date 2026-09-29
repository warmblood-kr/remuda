use remuda_core::agent::{AgentError, AgentProcess, AgentWriter, Cursor, Result, Size};
use remuda_core::input::{InputBatch, InputError, InputOutcome};
use remuda_core::{Clock, ManualClock, Session};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[test]
fn capture_completes_while_an_agent_write_is_stalled() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let session = Arc::new(Session::new(
        "blocked-writer",
        Box::new(BlockingAgent {
            writer: Arc::new(BlockingWriter {
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(release_rx),
                busy: AtomicBool::new(false),
                fail: false,
                refusal_once: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
            }),
        }),
        Arc::new(ManualClock::new()),
    ));
    let writer_session = Arc::clone(&session);
    let writer = thread::spawn(move || writer_session.send(b"blocked"));
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("agent write started");

    let capture_session = Arc::clone(&session);
    let (captured_tx, captured_rx) = mpsc::channel();
    let capture = thread::spawn(move || {
        captured_tx.send(capture_session.screen_text()).unwrap();
    });
    let capture_was_prompt = captured_rx.recv_timeout(Duration::from_millis(100)).is_ok();

    release_tx.send(()).unwrap();
    writer.join().unwrap().unwrap();
    if !capture_was_prompt {
        assert_eq!(
            captured_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
            "ready"
        );
    }
    capture.join().unwrap();
    assert!(capture_was_prompt, "capture waited for the blocked write");
}

#[test]
fn interactive_send_refuses_a_second_write_while_the_first_is_stalled() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        fail: false,
        refusal_once: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
    });
    let session = Arc::new(Session::new(
        "second-write",
        Box::new(BlockingAgent {
            writer: Arc::clone(&writer),
        }),
        Arc::new(ManualClock::new()),
    ));
    let first_session = Arc::clone(&session);
    let first = thread::spawn(move || first_session.send(b"first"));
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first write started");

    let second_session = Arc::clone(&session);
    let (second_tx, second_rx) = mpsc::channel();
    let second = thread::spawn(move || {
        let _ = second_tx.send(second_session.send(b"second"));
    });
    let second_result = second_rx.recv_timeout(Duration::from_millis(100));

    // Keep the regression case bounded too: if the second call queued behind
    // the first, the extra release lets it drain before the assertion fails.
    release_tx.send(()).unwrap();
    release_tx.send(()).unwrap();
    first.join().unwrap().unwrap();
    second.join().unwrap();
    assert!(matches!(second_result, Ok(Err(AgentError::Busy))));
    assert_eq!(writer.writes.load(Ordering::Acquire), 1);
}

#[test]
fn takeover_wakes_an_attached_write_without_holding_the_attach_slot() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        fail: false,
        refusal_once: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
    });
    let session = Arc::new(Session::new(
        "attached-takeover",
        Box::new(BlockingAgent {
            writer: Arc::clone(&writer),
        }),
        Arc::new(ManualClock::new()),
    ));
    let old_session = Arc::clone(&session);
    let (result_tx, result_rx) = mpsc::channel();
    let old_write = thread::spawn(move || {
        let held = old_session.attach();
        result_tx.send(held.write_raw(b"old key")).unwrap();
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("old attachment write started");

    let current = session.attach();
    assert!(current.generation() > 0);
    assert!(matches!(
        result_rx.recv_timeout(Duration::from_millis(100)),
        Ok(Err(AgentError::Attached))
    ));
    release_tx.send(()).unwrap();
    old_write.join().unwrap();
}

#[test]
fn failed_batch_retry_is_uncertain_while_writer_is_stalled_without_a_second_write() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        fail: true,
        refusal_once: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
    });
    let session = Arc::new(Session::new(
        "blocked-input",
        Box::new(BlockingAgent {
            writer: Arc::clone(&writer),
        }),
        Arc::new(ManualClock::new()),
    ));
    let first_session = Arc::clone(&session);
    let instance_id = session.instance_id().to_string();
    let first = thread::spawn(move || {
        first_session.apply_input_batch(InputBatch {
            instance_id: &instance_id,
            client_id: [7; 16],
            seq: 1,
            bytes: b"uncertain",
        })
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first input write started");

    assert_eq!(
        session
            .apply_input_batch(InputBatch {
                instance_id: session.instance_id(),
                client_id: [7; 16],
                seq: 1,
                bytes: b"uncertain",
            })
            .expect("retry result"),
        InputOutcome::Uncertain
    );
    release_tx.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap(), InputOutcome::Uncertain);
    assert_eq!(writer.writes.load(Ordering::Acquire), 1);
}

#[test]
fn definitely_refused_batch_reservations_are_released_for_retry() {
    let (started_tx, _started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        fail: false,
        refusal_once: AtomicUsize::new(1),
        writes: AtomicUsize::new(0),
    });
    let session = Session::new(
        "refused-input",
        Box::new(BlockingAgent {
            writer: Arc::clone(&writer),
        }),
        Arc::new(ManualClock::new()),
    );
    let batch = |client_id| InputBatch {
        instance_id: session.instance_id(),
        client_id,
        seq: 1,
        bytes: b"retry after refusal",
    };

    assert_eq!(
        session.apply_input_batch(batch([3; 16])),
        Err(InputError::Busy)
    );
    assert_eq!(writer.writes.load(Ordering::Acquire), 0);
    release_tx.send(()).unwrap();
    assert_eq!(
        session.apply_input_batch(batch([3; 16])),
        Ok(InputOutcome::Ack { duplicate: false })
    );

    writer.refusal_once.store(2, Ordering::Release);
    assert_eq!(
        session.apply_input_batch(batch([4; 16])),
        Ok(InputOutcome::Exited)
    );
    assert_eq!(writer.writes.load(Ordering::Acquire), 1);
    release_tx.send(()).unwrap();
    assert_eq!(
        session.apply_input_batch(batch([4; 16])),
        Ok(InputOutcome::Ack { duplicate: false })
    );
    assert_eq!(writer.writes.load(Ordering::Acquire), 2);
}

#[test]
fn late_success_after_timeout_does_not_turn_a_batch_retry_into_an_ack() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let writer = Arc::new(LateCompletionWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(Some(release_rx)),
        busy: Arc::new(AtomicBool::new(false)),
        writes: Arc::new(AtomicUsize::new(0)),
        finished: finished_tx,
    });
    let session = Session::new(
        "late-success",
        Box::new(LateCompletionAgent {
            writer: Arc::clone(&writer),
        }),
        Arc::new(ManualClock::new()),
    );
    let batch = InputBatch {
        instance_id: session.instance_id(),
        client_id: [5; 16],
        seq: 1,
        bytes: b"possibly late",
    };

    assert_eq!(
        session.apply_input_batch(batch),
        Ok(InputOutcome::Uncertain)
    );
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("underlying write remains in flight after timeout");
    assert!(writer.is_busy());
    assert_eq!(
        session.apply_input_batch(batch),
        Ok(InputOutcome::Uncertain)
    );
    assert_eq!(writer.writes.load(Ordering::Acquire), 0);

    release_tx.send(()).unwrap();
    finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("late write did not finish");
    assert!(!writer.is_busy());
    assert_eq!(writer.writes.load(Ordering::Acquire), 1);
    assert_eq!(
        session.apply_input_batch(batch),
        Ok(InputOutcome::Uncertain)
    );
    assert_eq!(writer.writes.load(Ordering::Acquire), 1);
}

#[test]
fn busy_remote_input_keeps_its_sequence_and_does_not_consume_rate_budget() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        fail: false,
        refusal_once: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
    });
    let session = Arc::new(Session::new(
        "busy-input",
        Box::new(BlockingAgent {
            writer: Arc::clone(&writer),
        }),
        Arc::new(ManualClock::new()),
    ));
    let send_session = Arc::clone(&session);
    let send = thread::spawn(move || send_session.send(b"local input"));
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("local write started");

    let bytes = vec![b'x'; remuda_core::input::MAX_INPUT_BYTES];
    let batch = InputBatch {
        instance_id: session.instance_id(),
        client_id: [6; 16],
        seq: 1,
        bytes: &bytes,
    };
    let rejected_session = Arc::clone(&session);
    let rejected_bytes = bytes.clone();
    let (rejected_tx, rejected_rx) = mpsc::channel();
    let rejected = thread::spawn(move || {
        let result = rejected_session.apply_input_batch(InputBatch {
            instance_id: rejected_session.instance_id(),
            client_id: [6; 16],
            seq: 1,
            bytes: &rejected_bytes,
        });
        let _ = rejected_tx.send(result);
    });
    let rejected_result = rejected_rx.recv_timeout(Duration::from_millis(100));

    // Four max-sized accepted batches exhaust the rate window. If the Busy
    // rejection charged it, one of these would be refused. Extra releases
    // bound the regression path if the rejected input queued behind the first.
    for _ in 0..6 {
        release_tx.send(()).unwrap();
    }
    send.join().unwrap().unwrap();
    rejected.join().unwrap();
    assert_eq!(rejected_result, Ok(Err(InputError::Busy)));
    for seq in 1..=4 {
        release_tx.send(()).unwrap();
        assert_eq!(
            session.apply_input_batch(InputBatch { seq, ..batch }),
            Ok(InputOutcome::Ack { duplicate: false })
        );
    }
    assert_eq!(writer.writes.load(Ordering::Acquire), 5);
}

#[test]
#[allow(clippy::too_many_lines)] // Exercises the feed-pause-to-stalled-write race end to end.
fn input_waiting_during_feed_pause_refuses_when_the_next_burst_stalls() {
    let (pause_started_tx, pause_started_rx) = mpsc::channel();
    let (pause_release_tx, pause_release_rx) = mpsc::channel();
    let clock = Arc::new(PausingClock {
        pause_started: Mutex::new(Some(pause_started_tx)),
        pause_release: Mutex::new(pause_release_rx),
    });
    let (write_started_tx, write_started_rx) = mpsc::channel();
    let (write_release_tx, write_release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(write_started_tx)),
        release: Mutex::new(write_release_rx),
        busy: AtomicBool::new(false),
        fail: false,
        refusal_once: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
    });
    let session = Arc::new(Session::new(
        "feed-stall-input",
        Box::new(BlockingAgent {
            writer: Arc::clone(&writer),
        }),
        clock.clone(),
    ));

    let feed_session = Arc::clone(&session);
    let feed = thread::spawn(move || {
        feed_session.feed(&[
            remuda_core::protocol::Step::Pause(100),
            remuda_core::protocol::Step::Burst(b"feed burst".to_vec()),
        ])
    });
    pause_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("feed entered its pause");

    let input_session = Arc::clone(&session);
    let (input_started_tx, input_started_rx) = mpsc::channel();
    let (input_result_tx, input_result_rx) = mpsc::channel();
    let input = thread::spawn(move || {
        let _ = input_started_tx.send(());
        let result = input_session.apply_input_batch(InputBatch {
            instance_id: input_session.instance_id(),
            client_id: [11; 16],
            seq: 1,
            bytes: b"remote batch",
        });
        let _ = input_result_tx.send(result);
    });
    input_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("remote input started");
    assert!(
        input_result_rx
            .recv_timeout(Duration::from_millis(25))
            .is_err(),
        "remote input must wait for the active feed act"
    );

    pause_release_tx.send(()).unwrap();
    write_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("feed burst is stalled in the writer");
    let input_result = input_result_rx.recv_timeout(Duration::from_millis(100));

    // Release enough writes to cleanly finish even if a regression queued the
    // remote batch behind the feed act instead of refusing it as Busy.
    write_release_tx.send(()).unwrap();
    write_release_tx.send(()).unwrap();
    feed.join().unwrap().unwrap();
    input.join().unwrap();
    assert_eq!(input_result, Ok(Err(InputError::Busy)));

    write_release_tx.send(()).unwrap();
    assert_eq!(
        session.apply_input_batch(InputBatch {
            instance_id: session.instance_id(),
            client_id: [11; 16],
            seq: 1,
            bytes: b"remote batch",
        }),
        Ok(InputOutcome::Ack { duplicate: false })
    );
    assert_eq!(writer.writes.load(Ordering::Acquire), 2);
}

struct BlockingAgent {
    writer: Arc<BlockingWriter>,
}

struct BlockingWriter {
    started: Mutex<Option<Sender<()>>>,
    release: Mutex<Receiver<()>>,
    busy: AtomicBool,
    fail: bool,
    /// Test-only refusals returned before a write is accepted: 1 = Busy,
    /// 2 = Exited.
    refusal_once: AtomicUsize,
    writes: AtomicUsize,
}

impl AgentWriter for BlockingWriter {
    fn write_bounded(&self, _bytes: &[u8]) -> Result<()> {
        match self.refusal_once.swap(0, Ordering::AcqRel) {
            1 => return Err(AgentError::Busy),
            2 => return Err(AgentError::Exited),
            _ => {}
        }
        self.writes.fetch_add(1, Ordering::AcqRel);
        self.busy.store(true, Ordering::Release);
        if let Some(started) = self.started.lock().unwrap().take() {
            started.send(()).unwrap();
        }
        self.release.lock().unwrap().recv().unwrap();
        self.busy.store(false, Ordering::Release);
        if self.fail {
            Err(AgentError::Io("test write failed".into()))
        } else {
            Ok(())
        }
    }

    fn write_to_completion_while(&self, _bytes: &[u8], cancelled: &dyn Fn() -> bool) -> Result<()> {
        self.writes.fetch_add(1, Ordering::AcqRel);
        self.busy.store(true, Ordering::Release);
        if let Some(started) = self.started.lock().unwrap().take() {
            started.send(()).unwrap();
        }
        loop {
            if cancelled() {
                return Err(AgentError::Attached);
            }
            match self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_millis(5))
            {
                Ok(()) => {
                    self.busy.store(false, Ordering::Release);
                    return if self.fail {
                        Err(AgentError::Io("test write failed".into()))
                    } else {
                        Ok(())
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(AgentError::Io("test writer released without signal".into()));
                }
            }
        }
    }

    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

impl AgentProcess for BlockingAgent {
    fn write(&mut self, _bytes: &[u8]) -> Result<()> {
        self.writer.write_bounded(_bytes)
    }

    fn input_writer(&mut self) -> Option<Arc<dyn AgentWriter>> {
        Some(Arc::clone(&self.writer) as Arc<dyn AgentWriter>)
    }

    fn screen_text(&mut self) -> Result<String> {
        Ok("ready".into())
    }

    fn cursor(&mut self) -> Result<Cursor> {
        Ok(Cursor {
            row: 0,
            col: 0,
            visible: true,
        })
    }

    fn is_alive(&mut self) -> bool {
        true
    }

    fn terminate(&mut self) -> Result<()> {
        Err(AgentError::Io("not used".into()))
    }

    fn size(&self) -> Size {
        Size::default()
    }
}

struct LateCompletionWriter {
    started: Mutex<Option<Sender<()>>>,
    release: Mutex<Option<Receiver<()>>>,
    busy: Arc<AtomicBool>,
    writes: Arc<AtomicUsize>,
    finished: Sender<()>,
}

impl AgentWriter for LateCompletionWriter {
    fn write_bounded(&self, _bytes: &[u8]) -> Result<()> {
        self.busy.store(true, Ordering::Release);
        if let Some(started) = self.started.lock().unwrap().take() {
            started.send(()).unwrap();
            let release = self.release.lock().unwrap().take().unwrap();
            let busy = Arc::clone(&self.busy);
            let writes = Arc::clone(&self.writes);
            let finished = self.finished.clone();
            thread::spawn(move || {
                release.recv().unwrap();
                writes.fetch_add(1, Ordering::AcqRel);
                busy.store(false, Ordering::Release);
                let _ = finished.send(());
            });
        }
        Err(AgentError::WriteTimeout {
            timeout: Duration::from_millis(1),
        })
    }

    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

struct LateCompletionAgent {
    writer: Arc<LateCompletionWriter>,
}

struct PausingClock {
    pause_started: Mutex<Option<Sender<()>>>,
    pause_release: Mutex<Receiver<()>>,
}

impl Clock for PausingClock {
    fn now(&self) -> Duration {
        Duration::ZERO
    }

    fn sleep(&self, duration: Duration) {
        if duration == Duration::from_millis(100) {
            if let Some(started) = self.pause_started.lock().unwrap().take() {
                let _ = started.send(());
            }
            let _ = self.pause_release.lock().unwrap().recv();
        }
    }
}

impl AgentProcess for LateCompletionAgent {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_bounded(bytes)
    }

    fn input_writer(&mut self) -> Option<Arc<dyn AgentWriter>> {
        Some(Arc::clone(&self.writer) as Arc<dyn AgentWriter>)
    }

    fn screen_text(&mut self) -> Result<String> {
        Ok(String::new())
    }

    fn cursor(&mut self) -> Result<Cursor> {
        Ok(Cursor {
            row: 0,
            col: 0,
            visible: true,
        })
    }

    fn is_alive(&mut self) -> bool {
        true
    }

    fn terminate(&mut self) -> Result<()> {
        Err(AgentError::Io("not used".into()))
    }

    fn size(&self) -> Size {
        Size::default()
    }
}
