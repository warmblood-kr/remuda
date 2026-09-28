use remuda_core::input::{
    validate_batch, InputDeduplicator, InputOutcome, INPUT_RING_CAPACITY, MAX_INPUT_BYTES,
};
use remuda_core::agent::{AgentError, AgentProcess, Cursor, Result, Size};
use remuda_core::{ManualClock, Session};
use std::sync::{Arc, Mutex};
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
    assert_eq!(live.apply_input_batch(&instance, [4; 16], 1, b"line"), InputOutcome::Ack { duplicate: false });
    assert_eq!(live.apply_input_batch("stale-instance", [4; 16], 1, b"line"), InputOutcome::WrongInstance);
    assert_eq!(writes.lock().unwrap().len(), 1);

    let exited = input_session(Arc::clone(&writes), false);
    assert_eq!(exited.apply_input_batch("stale-instance", [5; 16], 1, b"line"), InputOutcome::Exited);
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
                assert_eq!(session.apply_input_batch(session.instance_id(), client_id, 1, &[tag; 64]), InputOutcome::Ack { duplicate: false });
            })
        })
        .collect();
    for worker in threads {
        worker.join().expect("input worker completes");
    }
    let writes = writes.lock().unwrap();
    assert_eq!(writes.len(), 8);
    assert!(writes.iter().all(|batch| batch.len() == 64 && batch.iter().all(|byte| *byte == batch[0])));
}

fn input_session(writes: Arc<Mutex<Vec<Vec<u8>>>>, alive: bool) -> Session {
    Session::new(
        "input-test",
        Box::new(InputRecordingAgent { writes, alive }),
        Arc::new(ManualClock::new()),
    )
}

struct InputRecordingAgent {
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
    alive: bool,
}

impl AgentProcess for InputRecordingAgent {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.alive {
            return Err(AgentError::Exited);
        }
        self.writes.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }

    fn screen_text(&mut self) -> Result<String> { Ok(String::new()) }
    fn cursor(&mut self) -> Result<Cursor> { Ok(Cursor { row: 0, col: 0, visible: true }) }
    fn is_alive(&mut self) -> bool { self.alive }
    fn terminate(&mut self) -> Result<()> { self.alive = false; Ok(()) }
    fn size(&self) -> Size { Size::default() }
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
