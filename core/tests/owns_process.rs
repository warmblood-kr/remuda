//! Which session a process belongs to, asked of the backend rather than
//! derived from parent PIDs: the answer must survive the session's own child.

use remuda_core::agent::{AgentError, AgentProcess, Cursor, Result, Size};
use remuda_core::{ManualClock, Registry, ScriptedAgent, Session};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

struct FlappingAgent(Arc<AtomicUsize>);
impl AgentProcess for FlappingAgent {
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
        self.0.fetch_add(1, Ordering::SeqCst) != 0
    }
    fn process_id(&self) -> Option<u32> {
        Some(80)
    }
    fn owns_process(&self, pid: u32, _: Option<usize>) -> std::io::Result<bool> {
        Ok(pid == 80)
    }
    fn terminate(&mut self) -> Result<()> {
        Ok(())
    }
    fn size(&self) -> Size {
        Size::default()
    }
}

#[test]
fn first_observed_dead_session_never_revives_in_list_or_live_processes() {
    let registry = Registry::new();
    let checks = Arc::new(AtomicUsize::new(0));
    registry
        .register(Session::new(
            "flap",
            Box::new(FlappingAgent(checks)),
            Arc::new(ManualClock::new()),
        ))
        .unwrap();
    let instance_id = registry.get("flap").unwrap().instance_id().to_owned();

    assert!(!registry.list()[0].alive);
    assert_eq!(
        registry.validate_live_instance("flap", &instance_id),
        Err(remuda_core::registry::LiveInstanceError::NotLive)
    );
    assert!(registry.live_processes().is_empty());
    assert!(!registry.list()[0].alive);
    assert_eq!(
        registry.session_owning(80, None).unwrap().as_deref(),
        Some("flap")
    );
}

struct RefusingTerminateAgent(Arc<AtomicBool>);
impl AgentProcess for RefusingTerminateAgent {
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
        self.0.load(Ordering::SeqCst)
    }
    fn process_id(&self) -> Option<u32> {
        Some(81)
    }
    fn owns_process(&self, _: u32, _: Option<usize>) -> std::io::Result<bool> {
        Err(std::io::Error::other("ownership unavailable"))
    }
    fn terminate(&mut self) -> Result<()> {
        Err(AgentError::Io("termination refused".into()))
    }
    fn size(&self) -> Size {
        Size::default()
    }
}

#[test]
fn failed_terminate_does_not_latch_session_dead() {
    let registry = Registry::new();
    let alive = Arc::new(AtomicBool::new(true));
    registry
        .register(Session::new(
            "refuses",
            Box::new(RefusingTerminateAgent(Arc::clone(&alive))),
            Arc::new(ManualClock::new()),
        ))
        .unwrap();
    let session = registry.get("refuses").unwrap();

    assert!(registry.close("refuses").unwrap().is_err());
    assert!(alive.load(Ordering::SeqCst));
    assert!(session.is_alive());
    assert!(registry
        .validate_live_instance("refuses", session.instance_id())
        .is_ok());
    assert_eq!(registry.live_processes().len(), 1);
}

#[test]
fn a_live_owner_with_a_descendant_process_passes_runtime_validation() {
    let registry = Registry::new();
    let checks = Arc::new(AtomicUsize::new(1));
    let session = registry
        .register(Session::new(
            "live-owner",
            Box::new(FlappingAgent(checks)),
            Arc::new(ManualClock::new()),
        ))
        .unwrap();
    assert_eq!(
        registry.session_owning(80, None).unwrap().as_deref(),
        Some("live-owner")
    );
    assert!(registry
        .validate_live_instance("live-owner", session.instance_id())
        .is_ok());
}

#[test]
fn a_dead_owner_keeps_descendant_provenance_but_fails_runtime_validation() {
    let registry = Registry::new();
    let mut agent = ScriptedAgent::new(vec![]).owning(82);
    agent.kill();
    let session = registry
        .register(Session::new(
            "dead-owner",
            Box::new(agent),
            Arc::new(ManualClock::new()),
        ))
        .unwrap();
    assert_eq!(
        registry.session_owning(82, None).unwrap().as_deref(),
        Some("dead-owner")
    );
    assert_eq!(
        registry.validate_live_instance("dead-owner", session.instance_id()),
        Err(remuda_core::registry::LiveInstanceError::NotLive)
    );
}

#[test]
fn ownership_query_error_stays_unknown_instead_of_claiming_no_owner() {
    let registry = Registry::new();
    registry
        .register(Session::new(
            "unknown-owner",
            Box::new(RefusingTerminateAgent(Arc::new(AtomicBool::new(true)))),
            Arc::new(ManualClock::new()),
        ))
        .unwrap();
    assert_eq!(
        registry.session_owning(81, None).unwrap_err().to_string(),
        "session process membership unavailable"
    );
}

fn session(name: &str, agent: ScriptedAgent) -> Session {
    Session::new(
        name,
        Box::new(agent),
        Arc::new(remuda_core::ManualClock::new()),
    )
}

#[test]
fn the_session_whose_backend_holds_a_process_is_found_by_its_pid() {
    let registry = Registry::new();
    let plain = ScriptedAgent::new(vec![]);
    registry.register(session("plain", plain)).expect("plain");
    let owner = ScriptedAgent::new(vec![]).owning(40).owning(41);
    registry.register(session("owner", owner)).expect("owner");

    assert_eq!(
        registry.session_owning(41, None).unwrap().as_deref(),
        Some("owner")
    );
    assert_eq!(registry.session_owning(42, None).unwrap(), None);
}

#[test]
fn a_session_whose_child_exited_still_owns_its_other_processes() {
    let registry = Registry::new();
    let mut agent = ScriptedAgent::new(vec![]).owning(40);
    agent.kill();
    registry.register(session("gone", agent)).expect("gone");

    assert!(registry.live_processes().is_empty());
    assert_eq!(
        registry.session_owning(40, None).unwrap().as_deref(),
        Some("gone")
    );
}

#[test]
fn a_backend_that_holds_no_marker_owns_nothing() {
    let registry = Registry::new();
    let plain = ScriptedAgent::new(vec![]);
    registry.register(session("plain", plain)).expect("plain");
    assert_eq!(registry.session_owning(1, None).unwrap(), None);
}
