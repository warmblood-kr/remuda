use super::*;
use crate::agent::{AgentProcess, Cursor, Size};
use crate::clock::ManualClock;
use std::sync::mpsc;

struct ImmediateClock;
impl crate::clock::Clock for ImmediateClock {
    fn now(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&self, _: Duration) {}
}

struct ResizeAgent {
    events: Arc<Mutex<Vec<Size>>>,
    started: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
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
        Size::for_viewport(80, 24)
    }
    fn resize(&mut self, size: Size) -> Result<()> {
        let mut events = self.events.lock().unwrap();
        events.push(size);
        let first = events.len() == 1;
        drop(events);
        if first {
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
        }
        Ok(())
    }
}

#[test]
fn refresh_preserves_concurrent_resize_and_checks_instance() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let agent = ResizeAgent {
        events: events.clone(),
        started: started_tx,
        release: release_rx,
    };
    let session = Arc::new(Session::new(
        "test",
        Box::new(agent),
        Arc::new(ImmediateClock),
    ));
    assert!(!session.refresh_instance("wrong").unwrap());
    assert!(events.lock().unwrap().is_empty());
    let first = session.clone();
    let id = session.instance_id().to_owned();
    let refresh = std::thread::spawn(move || first.refresh_instance(&id).unwrap());
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let second = session.clone();
    let resize = std::thread::spawn(move || second.resize(Size::for_viewport(120, 24)).unwrap());
    release_tx.send(()).unwrap();
    assert!(refresh.join().unwrap());
    resize.join().unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            Size::for_viewport(81, 24),
            Size::for_viewport(80, 24),
            Size::for_viewport(120, 24)
        ]
    );
    assert_eq!(session.size(), Size::for_viewport(120, 24));
}

#[test]
fn registry_refresh_refuses_a_replaced_instance() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, _started_rx) = mpsc::channel();
    let (_release_tx, release_rx) = mpsc::channel();
    let agent = ResizeAgent {
        events: events.clone(),
        started: started_tx,
        release: release_rx,
    };
    let registry = crate::registry::Registry::new();
    let session = Session::new("test", Box::new(agent), Arc::new(ManualClock::new()));
    registry.register(session).ok().unwrap();
    assert!(!registry
        .refresh_instance("test", "old-instance")
        .unwrap()
        .unwrap());
    assert!(events.lock().unwrap().is_empty());
}

struct RestoreFailureAgent {
    calls: usize,
}
impl AgentProcess for RestoreFailureAgent {
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
        Size::for_viewport(512, 128)
    }
    fn resize(&mut self, size: Size) -> Result<()> {
        self.calls += 1;
        if self.calls == 1 && u32::from(size.cols()) * u32::from(size.rows()) > 65_536 {
            return Err(AgentError::Io("terminal area too large".into()));
        }
        if self.calls == 2 {
            return Err(AgentError::Io("restore failure".into()));
        }
        Ok(())
    }
}
#[test]
fn refresh_respects_host_area_limit_and_reports_restore_failure() {
    let session = Session::new(
        "test",
        Box::new(RestoreFailureAgent { calls: 0 }),
        Arc::new(ImmediateClock),
    );
    let error = session
        .refresh_instance(session.instance_id())
        .unwrap_err()
        .to_string();
    assert!(error.contains("restore"), "unexpected failure: {error}");
    assert_eq!(session.size(), Size::for_viewport(511, 128));
}
