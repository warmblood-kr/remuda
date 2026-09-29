use remuda_core::agent::{AgentError, AgentProcess, Cursor, Result, Size};
use remuda_core::input::{
    validate_batch, InputBatch, InputDeduplicator, InputOutcome, INPUT_RATE_BYTES_PER_SECOND,
    INPUT_RING_CAPACITY, MAX_INPUT_BYTES,
};
use remuda_core::{ManualClock, Session};
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
