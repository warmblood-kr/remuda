//! Which session a process belongs to, asked of the backend rather than
//! derived from parent PIDs: the answer must survive the session's own child.

use remuda_core::{Registry, ScriptedAgent, Session};
use std::sync::Arc;

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
