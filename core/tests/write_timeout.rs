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
                timed_out: AtomicBool::new(false),
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
fn interactive_send_queues_a_second_write_behind_a_healthy_write() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
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
    assert!(second_rx.recv_timeout(Duration::from_millis(25)).is_err());
    release_tx.send(()).unwrap();
    release_tx.send(()).unwrap();
    first.join().unwrap().unwrap();
    let second_result = second_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    second.join().unwrap();
    assert!(second_result.is_ok());
    assert_eq!(writer.writes.load(Ordering::Acquire), 2);
}

#[test]
fn interactive_send_returns_busy_only_after_the_writer_deadline() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
        fail: false,
        refusal_once: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
    });
    let session = Arc::new(Session::new(
        "timed-out-writer",
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
    assert!(second_rx.recv_timeout(Duration::from_millis(25)).is_err());
    writer.timed_out.store(true, Ordering::Release);
    assert!(matches!(
        second_rx.recv_timeout(Duration::from_secs(1)),
        Ok(Err(AgentError::Busy))
    ));
    release_tx.send(()).unwrap();
    first.join().unwrap().unwrap();
    second.join().unwrap();
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
        timed_out: AtomicBool::new(false),
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
    assert!(session.is_attached());
    release_tx.send(()).unwrap();
    old_write.join().unwrap();
    assert!(
        session.is_attached(),
        "old guard drop must not clear its successor"
    );
    drop(current);
    assert!(!session.is_attached());
}

#[test]
fn cancelled_attach_write_drops_the_current_attachment() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let session = Arc::new(Session::new(
        "detached-writer",
        Box::new(BlockingAgent {
            writer: Arc::new(BlockingWriter {
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(release_rx),
                busy: AtomicBool::new(false),
                timed_out: AtomicBool::new(false),
                fail: false,
                refusal_once: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
            }),
        }),
        Arc::new(ManualClock::new()),
    ));
    let detached = Arc::new(AtomicBool::new(false));
    let thread_session = Arc::clone(&session);
    let thread_detached = Arc::clone(&detached);
    let (result_tx, result_rx) = mpsc::channel();
    let write = thread::spawn(move || {
        let held = thread_session.attach();
        let result = held.write_raw_while(b"key", &|| thread_detached.load(Ordering::Acquire));
        let _ = result_tx.send(result);
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("attached write started");
    detached.store(true, Ordering::Release);
    let result = result_rx.recv_timeout(Duration::from_secs(1));
    let _ = release_tx.send(());
    write.join().unwrap();
    assert!(matches!(result, Ok(Err(AgentError::Attached))));
    assert!(
        !session.is_attached(),
        "disconnect cancellation releases the slot"
    );
}

#[test]
fn failed_batch_retry_is_uncertain_while_writer_is_stalled_without_a_second_write() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
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
        timed_out: AtomicBool::new(false),
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
#[allow(clippy::too_many_lines)]
fn batch_rejected_before_submit_is_busy_and_refunds_rate_for_retry() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
        fail: false,
        refusal_once: AtomicUsize::new(3),
        writes: AtomicUsize::new(0),
    });
    let session = Arc::new(Session::new(
        "batch-behind-timed-out-attach",
        Box::new(BlockingAgent {
            writer: Arc::clone(&writer),
        }),
        Arc::new(ManualClock::new()),
    ));
    let attach_session = Arc::clone(&session);
    let attach = thread::spawn(move || {
        let held = attach_session.attach();
        held.write_raw(b"human input")
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("attached write is active");

    let bytes = vec![b'x'; remuda_core::input::MAX_INPUT_BYTES];
    let batch = InputBatch {
        instance_id: session.instance_id(),
        client_id: [14; 16],
        seq: 1,
        bytes: &bytes,
    };
    assert_eq!(session.apply_input_batch(batch), Err(InputError::Busy));
    assert_eq!(writer.writes.load(Ordering::Acquire), 1);

    release_tx.send(()).unwrap();
    attach.join().unwrap().unwrap();
    for seq in 1..=4 {
        release_tx.send(()).unwrap();
        assert_eq!(
            session.apply_input_batch(InputBatch { seq, ..batch }),
            Ok(InputOutcome::Ack { duplicate: false })
        );
    }
    assert_eq!(
        session.apply_input_batch(InputBatch { seq: 5, ..batch }),
        Err(InputError::RateLimited)
    );
}

#[test]
fn abandoned_late_submit_batch_is_busy_and_can_be_retried() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
        fail: false,
        refusal_once: AtomicUsize::new(4),
        writes: AtomicUsize::new(0),
    });
    let session = Arc::new(Session::new(
        "abandoned-late-submit-batch",
        Box::new(BlockingAgent {
            writer: Arc::clone(&writer),
        }),
        Arc::new(ManualClock::new()),
    ));
    let instance_id = session.instance_id().to_owned();
    let batch = InputBatch {
        instance_id: &instance_id,
        client_id: [17; 16],
        seq: 1,
        bytes: b"never written",
    };

    assert_eq!(session.apply_input_batch(batch), Err(InputError::Busy));

    let retry_session = Arc::clone(&session);
    let retry_instance_id = instance_id.clone();
    let retry = thread::spawn(move || {
        retry_session.apply_input_batch(InputBatch {
            instance_id: &retry_instance_id,
            client_id: [17; 16],
            seq: 1,
            bytes: b"never written",
        })
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("retry write started");
    release_tx.send(()).unwrap();
    assert_eq!(
        retry.join().unwrap(),
        Ok(InputOutcome::Ack { duplicate: false })
    );
    assert_eq!(writer.writes.load(Ordering::Acquire), 1);
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
#[allow(clippy::too_many_lines)]
fn remote_input_waits_for_a_healthy_write_and_consumes_rate_budget_once() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        timed_out: AtomicBool::new(false),
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
    assert!(rejected_rx.recv_timeout(Duration::from_millis(25)).is_err());

    // A healthy write completes, then the queued batch lands once.
    for _ in 0..2 {
        release_tx.send(()).unwrap();
    }
    send.join().unwrap().unwrap();
    rejected.join().unwrap();
    assert_eq!(
        rejected_rx.try_recv().unwrap(),
        Ok(InputOutcome::Ack { duplicate: false })
    );
    for seq in 2..=4 {
        release_tx.send(()).unwrap();
        assert_eq!(
            session.apply_input_batch(InputBatch { seq, ..batch }),
            Ok(InputOutcome::Ack { duplicate: false })
        );
    }
    assert_eq!(writer.writes.load(Ordering::Acquire), 5);
    assert_eq!(
        session.apply_input_batch(InputBatch { seq: 5, ..batch }),
        Err(InputError::RateLimited)
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Exercises the feed-pause-to-stalled-write race end to end.
fn input_waiting_during_feed_pause_queues_behind_healthy_feed_write() {
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
        timed_out: AtomicBool::new(false),
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
    assert!(input_result_rx
        .recv_timeout(Duration::from_millis(25))
        .is_err());

    // Release the feed burst and the queued remote batch in order.
    write_release_tx.send(()).unwrap();
    write_release_tx.send(()).unwrap();
    feed.join().unwrap().unwrap();
    input.join().unwrap();
    assert_eq!(
        input_result_rx.try_recv().unwrap(),
        Ok(InputOutcome::Ack { duplicate: false })
    );
    assert_eq!(
        session.apply_input_batch(InputBatch {
            instance_id: session.instance_id(),
            client_id: [11; 16],
            seq: 1,
            bytes: b"remote batch",
        }),
        Ok(InputOutcome::Ack { duplicate: true })
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
    timed_out: AtomicBool,
    fail: bool,
    /// Test-only refusals returned before a write is accepted: 1 = Busy,
    /// 2 = Exited, 3 = an in-flight write timed out during submission,
    /// 4 = an abandoned late submit (the bytes were not written).
    refusal_once: AtomicUsize,
    writes: AtomicUsize,
}

impl AgentWriter for BlockingWriter {
    fn write_bounded(&self, _bytes: &[u8]) -> Result<()> {
        match self.refusal_once.swap(0, Ordering::AcqRel) {
            1 => return Err(AgentError::Busy),
            2 => return Err(AgentError::Exited),
            3 if self.busy.load(Ordering::Acquire) => {
                self.timed_out.store(true, Ordering::Release);
                return Err(AgentError::Busy);
            }
            4 => {
                return Err(AgentError::LateSubmitAbandoned {
                    bound: Duration::from_secs(30),
                });
            }
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

    fn is_timed_out(&self) -> bool {
        self.busy.load(Ordering::Acquire) && self.timed_out.load(Ordering::Acquire)
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
