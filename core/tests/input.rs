use remuda_core::agent::{AgentError, AgentProcess, Cursor, Result, Size};
use remuda_core::input::{
    validate_batch, InputBatch, InputDeduplicator, InputOutcome, INPUT_RATE_BYTES_PER_SECOND,
    INPUT_RING_CAPACITY, MAX_INPUT_BYTES,
};
use remuda_core::{ManualClock, Registry, Session};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

#[test]
fn retrying_a_recorded_batch_acknowledges_without_writing_twice() {
    let mut dedup = InputDeduplicator::new();
    let writes = Arc::new(Mutex::new(Vec::new()));
    assert_eq!(
        apply(&mut dedup, [1; 16], 1, &writes),
        InputOutcome::Ack { duplicate: false }
    );
    assert_eq!(
        apply(&mut dedup, [1; 16], 1, &writes),
        InputOutcome::Ack { duplicate: true }
    );
    assert_eq!(writes.lock().unwrap().len(), 1);
}

#[test]
fn retrying_an_evicted_batch_is_uncertain_and_does_not_write() {
    let mut dedup = InputDeduplicator::new();
    let writes = Arc::new(Mutex::new(Vec::new()));
    for seq in 1..=(INPUT_RING_CAPACITY as u64 + 1) {
        assert_eq!(
            apply(&mut dedup, [2; 16], seq, &writes),
            InputOutcome::Ack { duplicate: false }
        );
    }
    assert_eq!(
        apply(&mut dedup, [2; 16], 1, &writes),
        InputOutcome::Uncertain
    );
    assert_eq!(writes.lock().unwrap().len(), INPUT_RING_CAPACITY + 1);
}

#[test]
fn an_unknown_client_with_a_later_sequence_is_uncertain() {
    let mut dedup = InputDeduplicator::new();
    let writes = Arc::new(Mutex::new(Vec::new()));
    assert_eq!(
        apply(&mut dedup, [3; 16], 2, &writes),
        InputOutcome::Uncertain
    );
    assert!(writes.lock().unwrap().is_empty());
}

#[test]
fn evicting_a_client_forces_it_to_restart_at_sequence_one() {
    let mut dedup = InputDeduplicator::new();
    let writes = Arc::new(Mutex::new(Vec::new()));
    for client in 0..17 {
        assert_eq!(
            apply(&mut dedup, [client; 16], 1, &writes),
            InputOutcome::Ack { duplicate: false }
        );
    }
    assert_eq!(
        apply(&mut dedup, [0; 16], 2, &writes),
        InputOutcome::Uncertain
    );
    assert_eq!(writes.lock().unwrap().len(), 17);
}

#[test]
fn touching_a_client_moves_it_to_the_back_of_the_lru() {
    let mut dedup = InputDeduplicator::new();
    let writes = Arc::new(Mutex::new(Vec::new()));
    for client in 0..16 {
        assert_eq!(
            apply(&mut dedup, [client; 16], 1, &writes),
            InputOutcome::Ack { duplicate: false }
        );
    }
    assert_eq!(
        apply(&mut dedup, [0; 16], 1, &writes),
        InputOutcome::Ack { duplicate: true }
    );
    assert_eq!(
        apply(&mut dedup, [16; 16], 1, &writes),
        InputOutcome::Ack { duplicate: false }
    );
    assert_eq!(
        apply(&mut dedup, [0; 16], 2, &writes),
        InputOutcome::Ack { duplicate: false }
    );
    assert_eq!(
        apply(&mut dedup, [1; 16], 2, &writes),
        InputOutcome::Uncertain
    );
    assert_eq!(writes.lock().unwrap().len(), 18);
}

#[test]
fn the_rate_cap_is_per_session_bytes_per_second() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let clock = Arc::new(ManualClock::new());
    let session = Session::new(
        "rate-input",
        Box::new(InputRecordingAgent {
            writes,
            alive: true,
            fail_write: false,
            event_tx: None,
        }),
        clock.clone(),
    );
    for _ in 0..(INPUT_RATE_BYTES_PER_SECOND / MAX_INPUT_BYTES) {
        assert_eq!(session.check_rate(MAX_INPUT_BYTES), Ok(()));
    }
    assert_eq!(
        session.check_rate(1),
        Err(remuda_core::input::InputError::RateLimited)
    );
    clock.advance(std::time::Duration::from_secs(1));
    assert_eq!(session.check_rate(1), Ok(()));
}

#[test]
fn a_rate_limited_batch_does_not_enter_dedup_history() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let clock = Arc::new(ManualClock::new());
    let session = Session::new(
        "rate-batch-input",
        Box::new(InputRecordingAgent {
            writes: Arc::clone(&writes),
            alive: true,
            fail_write: false,
            event_tx: None,
        }),
        clock.clone(),
    );
    let instance_id = session.instance_id().to_string();
    let bytes = vec![b'x'; MAX_INPUT_BYTES];
    for seq in 1..=4 {
        assert_eq!(
            session.apply_input_batch(InputBatch {
                instance_id: &instance_id,
                client_id: [11; 16],
                seq,
                bytes: &bytes,
            }),
            Ok(InputOutcome::Ack { duplicate: false })
        );
    }
    assert_eq!(
        session.apply_input_batch(InputBatch {
            instance_id: &instance_id,
            client_id: [11; 16],
            seq: 5,
            bytes: &bytes,
        }),
        Err(remuda_core::input::InputError::RateLimited)
    );
    assert_eq!(writes.lock().unwrap().len(), 4);

    clock.advance(std::time::Duration::from_secs(1));
    assert_eq!(
        session.apply_input_batch(InputBatch {
            instance_id: &instance_id,
            client_id: [11; 16],
            seq: 5,
            bytes: &bytes,
        }),
        Ok(InputOutcome::Ack { duplicate: false })
    );
    assert_eq!(writes.lock().unwrap().len(), 5);
}

#[test]
fn malformed_client_ids_and_oversize_batches_are_rejected() {
    assert!(validate_batch("not-an-id", 1, b"line").is_err());
    assert!(validate_batch(&"00".repeat(16), 0, b"line").is_err());
    assert!(validate_batch(&"00".repeat(16), 1, &vec![0; MAX_INPUT_BYTES + 1]).is_err());
    assert!(validate_batch(&"00".repeat(16), 1, b"").is_err());
    assert!(validate_batch(&"ab".repeat(16), 1, b"line").is_ok());
}

#[test]
fn wrong_instance_precedes_duplicate_lookup_and_exited_sessions_are_refused() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let live = input_session(Arc::clone(&writes), true);
    let instance = live.instance_id().to_string();
    assert_eq!(
        live.apply_input_batch(InputBatch {
            instance_id: &instance,
            client_id: [4; 16],
            seq: 1,
            bytes: b"line",
        }),
        Ok(InputOutcome::Ack { duplicate: false })
    );
    assert_eq!(
        live.apply_input_batch(InputBatch {
            instance_id: "stale-instance",
            client_id: [4; 16],
            seq: 1,
            bytes: b"line",
        }),
        Ok(InputOutcome::WrongInstance)
    );
    assert_eq!(writes.lock().unwrap().len(), 1);

    let exited = input_session(Arc::clone(&writes), false);
    assert_eq!(
        exited.apply_input_batch(InputBatch {
            instance_id: "stale-instance",
            client_id: [5; 16],
            seq: 1,
            bytes: b"line",
        }),
        Ok(InputOutcome::Exited)
    );
    assert_eq!(writes.lock().unwrap().len(), 1);
}

#[test]
fn concurrent_batches_are_written_as_atomic_pieces() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let session = Arc::new(input_session(Arc::clone(&writes), true));
    let threads: Vec<_> = (0..8u8)
        .map(|tag| {
            let session = Arc::clone(&session);
            thread::spawn(move || {
                let client_id = [tag; 16];
                assert_eq!(
                    session.apply_input_batch(InputBatch {
                        instance_id: session.instance_id(),
                        client_id,
                        seq: 1,
                        bytes: &[tag; 64],
                    }),
                    Ok(InputOutcome::Ack { duplicate: false })
                );
            })
        })
        .collect();
    for worker in threads {
        worker.join().expect("input worker completes");
    }
    let writes = writes.lock().unwrap();
    assert_eq!(writes.len(), 8);
    assert!(writes
        .iter()
        .all(|batch| batch.len() == 64 && batch.iter().all(|byte| *byte == batch[0])));
}

#[test]
fn invalid_batches_are_errors_and_never_write() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let session = input_session(Arc::clone(&writes), true);
    for (seq, bytes) in [
        (0, b"line".to_vec()),
        (1, Vec::new()),
        (1, vec![0; MAX_INPUT_BYTES + 1]),
    ] {
        assert!(session
            .apply_input_batch(InputBatch {
                instance_id: session.instance_id(),
                client_id: [8; 16],
                seq,
                bytes: &bytes,
            })
            .is_err());
    }
    assert!(writes.lock().unwrap().is_empty());
}

#[test]
fn a_failed_write_and_its_retry_are_uncertain_without_recorded_writes() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let session = Session::new(
        "failed-input",
        Box::new(InputRecordingAgent {
            writes: Arc::clone(&writes),
            alive: true,
            fail_write: true,
            event_tx: None,
        }),
        Arc::new(ManualClock::new()),
    );
    let batch = InputBatch {
        instance_id: session.instance_id(),
        client_id: [9; 16],
        seq: 1,
        bytes: b"lost",
    };
    assert_eq!(
        session.apply_input_batch(batch),
        Ok(InputOutcome::Uncertain)
    );
    assert_eq!(
        session.apply_input_batch(batch),
        Ok(InputOutcome::Uncertain)
    );
    assert!(writes.lock().unwrap().is_empty());
}

#[test]
fn a_batch_cannot_interleave_with_a_concurrent_feed() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (event_tx, event_rx) = mpsc::channel();
    let clock = Arc::new(ManualClock::new());
    let session = Arc::new(Session::new(
        "feed-input-atomicity",
        Box::new(InputRecordingAgent {
            writes: Arc::clone(&writes),
            alive: true,
            fail_write: false,
            event_tx: Some(event_tx),
        }),
        clock.clone(),
    ));
    let feed_session = Arc::clone(&session);
    let feed = thread::spawn(move || {
        feed_session
            .feed(&[
                remuda_core::protocol::Step::Burst(b"feed-before".to_vec()),
                remuda_core::protocol::Step::Pause(100),
                remuda_core::protocol::Step::Burst(b"feed-after".to_vec()),
            ])
            .expect("feed act succeeds");
    });
    expect_input_event(&event_rx, b"feed-before");
    let batch_session = Arc::clone(&session);
    let batch = thread::spawn(move || {
        batch_session
            .apply_input_batch(InputBatch {
                instance_id: batch_session.instance_id(),
                client_id: [10; 16],
                seq: 1,
                bytes: b"batch",
            })
            .expect("batch succeeds");
    });
    assert_no_input_event(&event_rx);
    clock.advance(std::time::Duration::from_millis(100));
    expect_input_event(&event_rx, b"feed-after");
    expect_input_event(&event_rx, b"batch");
    feed.join().unwrap();
    batch.join().unwrap();
    assert_eq!(
        *writes.lock().unwrap(),
        vec![
            b"feed-before".to_vec(),
            b"feed-after".to_vec(),
            b"batch".to_vec()
        ]
    );
}

#[test]
fn a_send_sleeps_through_a_feed_pause_then_wakes_when_the_act_releases() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (event_tx, event_rx) = mpsc::channel();
    let clock = Arc::new(ManualClock::new());
    let session = Arc::new(Session::new(
        "send-after-feed-pause",
        Box::new(InputRecordingAgent {
            writes: Arc::clone(&writes),
            alive: true,
            fail_write: false,
            event_tx: Some(event_tx),
        }),
        clock.clone(),
    ));
    let feed_session = Arc::clone(&session);
    let feed = thread::spawn(move || {
        feed_session
            .feed(&[
                remuda_core::protocol::Step::Burst(b"feed-before".to_vec()),
                remuda_core::protocol::Step::Pause(100),
                remuda_core::protocol::Step::Burst(b"feed-after".to_vec()),
            ])
            .expect("feed act succeeds");
    });
    expect_input_event(&event_rx, b"feed-before");

    let send_session = Arc::clone(&session);
    let (send_tx, send_rx) = mpsc::channel();
    let send = thread::spawn(move || send_tx.send(send_session.send(b"competing")).unwrap());
    assert!(send_rx
        .recv_timeout(std::time::Duration::from_millis(40))
        .is_err());
    assert_no_input_event(&event_rx);

    clock.advance(std::time::Duration::from_millis(100));
    expect_input_event(&event_rx, b"feed-after");
    expect_input_event(&event_rx, b"competing");
    assert!(matches!(
        send_rx.recv_timeout(std::time::Duration::from_secs(1)),
        Ok(Ok(()))
    ));
    feed.join().unwrap();
    send.join().unwrap();
    assert_eq!(
        *writes.lock().unwrap(),
        vec![
            b"feed-before".to_vec(),
            b"feed-after".to_vec(),
            b"competing".to_vec()
        ]
    );
}

#[test]
fn an_instance_bound_key_waits_through_a_feed_pause_without_interleaving() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (event_tx, event_rx) = mpsc::channel();
    let clock = Arc::new(ManualClock::new());
    let session = Arc::new(Session::new(
        "bound-key-after-feed-pause",
        Box::new(InputRecordingAgent {
            writes: Arc::clone(&writes),
            alive: true,
            fail_write: false,
            event_tx: Some(event_tx),
        }),
        clock.clone(),
    ));
    let expected_instance = session.instance_id().to_owned();
    let feed_session = Arc::clone(&session);
    let feed = thread::spawn(move || {
        feed_session
            .feed(&[
                remuda_core::protocol::Step::Burst(b"feed-before".to_vec()),
                remuda_core::protocol::Step::Pause(100),
                remuda_core::protocol::Step::Burst(b"feed-after".to_vec()),
            ])
            .expect("feed act succeeds");
    });
    expect_input_event(&event_rx, b"feed-before");

    let key_session = Arc::clone(&session);
    let (key_tx, key_rx) = mpsc::channel();
    let key = thread::spawn(move || {
        key_tx
            .send(key_session.send_key_if_instance(&expected_instance, b"\x1b[A"))
            .unwrap();
    });
    assert!(key_rx
        .recv_timeout(std::time::Duration::from_millis(40))
        .is_err());
    assert_no_input_event(&event_rx);

    clock.advance(std::time::Duration::from_millis(100));
    expect_input_event(&event_rx, b"feed-after");
    expect_input_event(&event_rx, b"\x1b[A");
    assert_eq!(
        key_rx.recv_timeout(std::time::Duration::from_secs(1)),
        Ok(Ok(()))
    );
    feed.join().unwrap();
    key.join().unwrap();
    assert_eq!(
        *writes.lock().unwrap(),
        vec![
            b"feed-before".to_vec(),
            b"feed-after".to_vec(),
            b"\x1b[A".to_vec()
        ]
    );
}

fn expect_input_event(receiver: &mpsc::Receiver<Vec<u8>>, expected: &[u8]) {
    assert_eq!(
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        expected
    );
}

fn assert_no_input_event(receiver: &mpsc::Receiver<Vec<u8>>) {
    assert!(receiver
        .recv_timeout(std::time::Duration::from_millis(40))
        .is_err());
}

#[test]
fn legacy_opt_out_does_not_block_trusted_remote_input() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let registry = Registry::new();
    let session = registry
        .register(input_session(Arc::clone(&writes), true))
        .expect("register");
    let instance = session.instance_id().to_string();
    let remote = |seq, bytes: &'static [u8]| InputBatch {
        instance_id: &instance,
        client_id: [9; 16],
        seq,
        bytes,
    };
    assert!(matches!(
        registry.apply_remote_input_batch("input-test", remote(1, b"a"), false),
        Some(Ok(InputOutcome::Ack { duplicate: false }))
    ));
    assert_eq!(
        registry.set_remote_input_opt_out("input-test", &instance, true),
        Some(())
    );
    assert!(matches!(
        registry.apply_remote_input_batch("input-test", remote(2, b"b"), false),
        Some(Ok(InputOutcome::Ack { duplicate: false }))
    ));
    assert!(registry
        .list()
        .iter()
        .any(|row| row.name == "input-test" && !row.remote_input_disabled));
    let local = InputBatch {
        instance_id: &instance,
        client_id: [8; 16],
        seq: 1,
        bytes: b"c",
    };
    assert!(matches!(
        registry.apply_input_batch("input-test", local),
        Some(Ok(InputOutcome::Ack { duplicate: false }))
    ));
    assert_eq!(writes.lock().unwrap().len(), 3);
    assert_eq!(
        registry.set_remote_input_opt_out("input-test", "not-the-instance", false),
        None
    );
}

#[test]
fn legacy_opt_out_does_not_interrupt_or_block_later_input() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let registry = Arc::new(Registry::new());
    let session = registry
        .register(Session::new(
            "input-test",
            Box::new(StallingAgent {
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(release_rx),
                released: std::sync::atomic::AtomicBool::new(false),
            }),
            Arc::new(ManualClock::new()),
        ))
        .expect("register");
    let instance = session.instance_id().to_string();
    let admitted_instance = instance.clone();
    let admitted_registry = Arc::clone(&registry);
    let admitted = thread::spawn(move || {
        admitted_registry.apply_remote_input_batch(
            "input-test",
            InputBatch {
                instance_id: &admitted_instance,
                client_id: [4; 16],
                seq: 1,
                bytes: b"admitted",
            },
            true,
        )
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("admitted line reached the writer");
    assert_eq!(
        registry.set_remote_input_opt_out("input-test", &instance, true),
        Some(()),
        "off must not wait for an admitted write outside the registry lock"
    );
    release_tx.send(()).expect("release writer");
    let outcome = admitted.join().unwrap();
    assert!(matches!(
        outcome,
        Some(Ok(InputOutcome::Ack { duplicate: false }))
    ));
    assert!(matches!(
        registry.apply_remote_input_batch(
            "input-test",
            InputBatch {
                instance_id: &instance,
                client_id: [5; 16],
                seq: 1,
                bytes: b"after",
            },
            false
        ),
        Some(Ok(InputOutcome::Ack { duplicate: false }))
    ));
}

#[test]
fn remote_input_line_adds_exactly_one_return_and_a_duplicate_adds_none() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let registry = Registry::new();
    let session = registry
        .register(input_session(Arc::clone(&writes), true))
        .expect("register");
    let batch = InputBatch {
        instance_id: session.instance_id(),
        client_id: [7; 16],
        seq: 1,
        bytes: b"hello",
    };
    assert!(matches!(
        registry.apply_remote_input_batch("input-test", batch, true),
        Some(Ok(InputOutcome::Ack { duplicate: false }))
    ));
    let written: Vec<u8> = writes
        .lock()
        .unwrap()
        .iter()
        .flat_map(|chunk| chunk.iter().copied())
        .collect();
    assert!(
        written.ends_with(b"hello\r"),
        "line writes text then one Return: {written:?}"
    );
    let again = InputBatch {
        instance_id: session.instance_id(),
        client_id: [7; 16],
        seq: 1,
        bytes: b"hello",
    };
    assert!(matches!(
        registry.apply_remote_input_batch("input-test", again, true),
        Some(Ok(InputOutcome::Ack { duplicate: true }))
    ));
    assert_eq!(writes.lock().unwrap().len(), 1, "duplicate writes no bytes");
}

#[test]
fn legacy_launch_time_opt_out_is_ignored() {
    let registry = Registry::new();
    let session = registry
        .register_with_remote_input_disabled(
            input_session(Arc::new(Mutex::new(Vec::new())), true),
            true,
        )
        .expect("register protected");
    assert!(!registry.remote_input_disabled(session.instance_id()));
    assert!(!registry.list()[0].remote_input_disabled);
    assert!(matches!(
        registry.apply_remote_input_batch(
            "input-test",
            InputBatch {
                instance_id: session.instance_id(),
                client_id: [6; 16],
                seq: 1,
                bytes: b"blocked",
            },
            false,
        ),
        Some(Ok(InputOutcome::Ack { duplicate: false }))
    ));
}

#[test]
fn remote_input_opt_out_does_not_follow_a_reused_name() {
    let registry = Registry::new();
    let first = registry
        .register(input_session(Arc::new(Mutex::new(Vec::new())), true))
        .expect("register");
    let instance = first.instance_id().to_string();
    assert_eq!(
        registry.set_remote_input_opt_out("input-test", &instance, true),
        Some(())
    );
    registry.remove("input-test");
    assert!(
        !registry.remote_input_disabled(&instance),
        "removal prunes the exact-instance policy"
    );
    let replacement = registry
        .register(input_session(Arc::new(Mutex::new(Vec::new())), true))
        .expect("replacement");
    assert!(!registry.remote_input_disabled(replacement.instance_id()));
    assert!(!registry.list()[0].remote_input_disabled);
}

fn input_session(writes: Arc<Mutex<Vec<Vec<u8>>>>, alive: bool) -> Session {
    Session::new(
        "input-test",
        Box::new(InputRecordingAgent {
            writes,
            alive,
            fail_write: false,
            event_tx: None,
        }),
        Arc::new(ManualClock::new()),
    )
}

/// A writer that reports when a line arrived and waits for release, so the
/// linearization of an admitted write against a later opt-out is deterministic.
struct StallingAgent {
    started: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
    released: std::sync::atomic::AtomicBool,
}

impl AgentProcess for StallingAgent {
    fn write(&mut self, _bytes: &[u8]) -> Result<()> {
        if let Some(started) = self.started.lock().unwrap().take() {
            let _ = started.send(());
        }
        if !self
            .released
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(1))
                .map_err(|_| AgentError::Io("stalled writer was not released".into()))?;
        }
        Ok(())
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

    fn size(&self) -> Size {
        Size::new(80, 24)
    }

    fn is_alive(&mut self) -> bool {
        true
    }

    fn terminate(&mut self) -> Result<()> {
        Ok(())
    }
}

struct InputRecordingAgent {
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
    alive: bool,
    fail_write: bool,
    event_tx: Option<mpsc::Sender<Vec<u8>>>,
}

impl AgentProcess for InputRecordingAgent {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.alive {
            return Err(AgentError::Exited);
        }
        if self.fail_write {
            return Err(AgentError::Io("injected write failure".into()));
        }
        self.writes.lock().unwrap().push(bytes.to_vec());
        if let Some(sender) = &self.event_tx {
            sender.send(bytes.to_vec()).unwrap();
        }
        Ok(())
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
        self.alive
    }
    fn terminate(&mut self) -> Result<()> {
        self.alive = false;
        Ok(())
    }
    fn size(&self) -> Size {
        Size::default()
    }
}

fn apply(
    dedup: &mut InputDeduplicator,
    client: [u8; 16],
    seq: u64,
    writes: &Arc<Mutex<Vec<Vec<u8>>>>,
) -> InputOutcome {
    dedup.apply(client, seq, || {
        writes.lock().unwrap().push(seq.to_be_bytes().to_vec());
        Ok(())
    })
}

#[test]
fn input_line_delivers_one_return_and_deduplicates_with_or_without_composer_cr() {
    for bytes in [b"hello".as_slice(), b"hello\r", b"\r"] {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let session = input_session(writes.clone(), true);
        let batch = InputBatch {
            instance_id: session.instance_id(),
            client_id: [7; 16],
            seq: 1,
            bytes,
        };
        assert_eq!(
            session.apply_input_line_batch(batch).unwrap(),
            InputOutcome::Ack { duplicate: false }
        );
        assert_eq!(
            session.apply_input_line_batch(batch).unwrap(),
            InputOutcome::Ack { duplicate: true }
        );
        let delivered: Vec<u8> = writes.lock().unwrap().iter().flatten().copied().collect();
        assert_eq!(
            delivered,
            if bytes == b"\r" {
                b"\r".to_vec()
            } else {
                b"hello\r".to_vec()
            }
        );
        assert_eq!(delivered.iter().filter(|b| **b == b'\r').count(), 1);
    }
}

#[test]
fn input_line_refuses_wrong_instance_and_invalid_utf8_without_writing() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let session = input_session(writes.clone(), true);
    let batch = InputBatch {
        instance_id: "other",
        client_id: [8; 16],
        seq: 1,
        bytes: b"hello\r",
    };
    assert_eq!(
        session.apply_input_line_batch(batch).unwrap(),
        InputOutcome::WrongInstance
    );
    assert!(session
        .apply_input_line_batch(InputBatch {
            instance_id: session.instance_id(),
            bytes: &[255],
            ..batch
        })
        .is_err());
    assert!(writes.lock().unwrap().is_empty());
}

#[test]
fn instance_pinned_resize_reaches_backend_and_stale_instance_writes_nothing() {
    struct ResizeAgent(Arc<Mutex<Vec<Size>>>);
    impl AgentProcess for ResizeAgent {
        fn write(&mut self, _: &[u8]) -> Result<()> {
            Ok(())
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
            Ok(())
        }
        fn size(&self) -> Size {
            Size::default()
        }
        fn resize(&mut self, size: Size) -> Result<()> {
            self.0.lock().unwrap().push(size);
            Ok(())
        }
    }
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let session = Session::new(
        "resize",
        Box::new(ResizeAgent(sizes.clone())),
        Arc::new(ManualClock::new()),
    );
    let size = Size::for_viewport(31, 9);
    assert!(!session.resize_instance("stale", size).unwrap());
    assert!(sizes.lock().unwrap().is_empty());
    assert!(session
        .resize_instance(session.instance_id(), size)
        .unwrap());
    assert_eq!(*sizes.lock().unwrap(), vec![size]);
    assert!(session
        .resize_instance(session.instance_id(), Size::new(2000, 600))
        .is_err());
    assert_eq!(*sizes.lock().unwrap(), vec![size]);
}
