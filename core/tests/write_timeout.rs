use remuda_core::agent::{AgentError, AgentProcess, AgentWriter, Cursor, Result, Size};
use remuda_core::input::{InputBatch, InputOutcome};
use remuda_core::{ManualClock, Session};
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
fn failed_batch_retry_is_uncertain_while_writer_is_stalled_without_a_second_write() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = Arc::new(BlockingWriter {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        busy: AtomicBool::new(false),
        fail: true,
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

struct BlockingAgent {
    writer: Arc<BlockingWriter>,
}

struct BlockingWriter {
    started: Mutex<Option<Sender<()>>>,
    release: Mutex<Receiver<()>>,
    busy: AtomicBool,
    fail: bool,
    writes: AtomicUsize,
}

impl AgentWriter for BlockingWriter {
    fn write_bounded(&self, _bytes: &[u8]) -> Result<()> {
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
